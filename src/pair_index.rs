//! The verified-pair index: content-mode evidence kept across runs (perf-design §2).
//!
//! After a run proves a pair equal with two live MD5s, the facts both sides had when it did so are
//! stored with the digest. A later run whose fresh facts still match exactly, on both sides, takes
//! the digest instead of reading either side again. A no-change content run then costs the
//! listing, not a read of the whole share on both NASes.
//!
//! What may be stored, and what may be served, is decided here, independently of where records
//! live: [`PairIndex`] is the storage interface, [`MemoryPairIndex`] the in-memory one the rules are
//! tested against, and [`crate::pair_index_redb`] the one runs use. The rules (perf-design
//! §2.4-2.6, §2.9):
//!
//! - **Served** only when the index is bound to this profile, source, remote root and server; the
//!   pair is in this run's comparison set under exactly the stored spelling; size, mtime, ctime,
//!   device and inode are unchanged locally and size, mtime, ctime and crtime remotely (or the
//!   remote side is live, in local-only mode); the record is not racy and not expired; it is not
//!   drawn as a canary; and the run's link guard ran everywhere.
//! - **Stored** only from pairs whose two digests this run computed live and which agreed. Served
//!   pairs keep their original verification time, so `max_age` expires them.
//! - **Never** the cause of an upload, a deletion or a server copy: a served digest is MD5-only
//!   (see [`crate::digest_evidence`]), and the only error it can make is a missed upload of a pair
//!   whose every fact held, which the canary and rolling expiry bound.
//! - Every way the index cannot be used is a named issue and a fully live run; none fails a run or
//!   makes it partial.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::{RemoteEntry, RemoteInventory};
use crate::digest_evidence::{
    EvidenceReport, EvidenceSource, TouchedPaths, Vouched, VouchedPair, split_canaries,
};
use crate::local::{EntryKind, LocalEntry, LocalInventory};

/// The `meta.schema` every index carries.
pub const SCHEMA: &str = "sdsync.pair-index.v1";

/// Bumped whenever what makes two files "the same" changes (`compare_files`,
/// `comparison_candidates`, the record's fields). An index written under another epoch is retired.
pub const SEMANTICS_EPOCH: u32 = 1;

/// Default age after which evidence is re-verified, in seconds (30 days).
pub const DEFAULT_MAX_AGE_SECONDS: i64 = 30 * 86_400;

/// A record whose change time is within this many seconds of the clock that observed it may have
/// been written during the observation, so it is stored racy and never served.
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;
const RACY_WINDOW_SECONDS: i64 = 2;

/// What an index is bound to. A mismatch in any field retires it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    pub profile: String,
    /// The source root, canonical.
    pub source: String,
    /// The device the source root is on.
    pub source_device: u64,
    /// The remote root.
    pub remote: String,
    /// The endpoint host plus the File Station hostname that answered.
    pub server: String,
}

/// Whether the remote side's change time can be trusted to reveal a replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteSignal {
    /// DSM reports `ctime`, and no self-test has shown it to stand still on a write.
    Ctime,
    /// It cannot: only the local read is skipped; File Station's MD5 always runs.
    LocalOnly,
}

impl RemoteSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ctime => "ctime",
            Self::LocalOnly => "local-only",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ctime" => Some(Self::Ctime),
            "local-only" => Some(Self::LocalOnly),
            _ => None,
        }
    }
}

/// The `meta` table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexMeta {
    pub schema: String,
    pub semantics_epoch: u32,
    pub binding: Binding,
    pub remote_signal: RemoteSignal,
    /// When a canary last ran against this index (Unix seconds).
    pub canary_at: i64,
}

/// One stored pair: the facts both sides had when the pair was proven equal, and the digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PairRecord {
    pub size: u64,
    pub local_mtime_ns: i64,
    pub local_ctime_ns: i64,
    pub device: u64,
    pub inode: u64,
    pub remote_mtime: i64,
    pub remote_ctime: Option<i64>,
    pub remote_crtime: Option<i64>,
    pub md5: [u8; 16],
    /// Local clock when the local read began (Unix seconds).
    pub verified_at: i64,
    /// NAS clock from the `MD5.start` that read the remote copy, when it sent one.
    pub verified_at_nas: Option<i64>,
    /// Observed within the racy window on either side: stored, never served.
    pub racy: bool,
}

/// A stored pair and the remote spelling it was proven under, when it differs from the local one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPair {
    pub record: PairRecord,
    pub remote_spelling: Option<String>,
}

/// How a storage backend failed, named for the result, the log and the dashboard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexIssue {
    /// `pair-index-locked`, `pair-index-corrupt`, `pair-index-store-failed`, ...
    pub code: &'static str,
    pub severity: Severity,
    /// The index file or directory concerned, or the entry for an entry-level condition.
    pub path: String,
    pub cause: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Info,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
        }
    }
}

/// Every issue code the index can name: the closed vocabulary `docs/observability.schema.json`
/// publishes (`$defs.pairIndex`) and the dashboard renders.
pub const ISSUE_CODES: &[&str] = &[
    "pair-index-locked",
    "pair-index-readonly",
    "pair-index-corrupt",
    "pair-index-format-unsupported",
    "pair-index-store-failed",
    "pair-index-unsafe",
    "pair-index-directory-unusable",
    "pair-index-unbound",
    "pair-index-canary-mismatch",
    "pair-index-remote-ctime-static",
    "pair-index-retired",
    "pair-index-compacted",
    "pair-index-compaction-failed",
    "pair-index-not-applicable",
    "pair-index-unsupported-platform",
    "pair-index-link-guard-unchecked",
    "pair-index-remote-ctime-unavailable",
];

impl IndexIssue {
    pub fn warning(code: &'static str, path: impl Into<String>, cause: impl Into<String>) -> Self {
        debug_assert!(ISSUE_CODES.contains(&code), "unpublished issue code {code}");
        Self {
            code,
            severity: Severity::Warning,
            path: path.into(),
            cause: cause.into(),
        }
    }

    pub fn info(code: &'static str, path: impl Into<String>, cause: impl Into<String>) -> Self {
        debug_assert!(ISSUE_CODES.contains(&code), "unpublished issue code {code}");
        Self {
            code,
            severity: Severity::Info,
            path: path.into(),
            cause: cause.into(),
        }
    }
}

/// One end-of-run write: everything is applied in one transaction, or nothing is.
#[derive(Clone, Debug, Default)]
pub struct IndexCommit {
    /// Remove every pair first (a retired binding, a canary disagreement, `--pair-index-refresh`).
    pub clear: bool,
    /// Pairs to remove.
    pub remove: Vec<String>,
    /// Pairs to write, by local relative path.
    pub upsert: Vec<(String, StoredPair)>,
    /// The `meta` to stamp.
    pub meta: Option<IndexMeta>,
}

/// Where verified pairs are kept. A backend reads in one window and writes once, at the end.
pub trait PairIndex {
    /// The stored `meta`, or `None` for an empty index.
    fn meta(&mut self) -> Result<Option<IndexMeta>, IndexIssue>;
    /// The pair stored under `local_relative`.
    fn get(&mut self, local_relative: &str) -> Result<Option<StoredPair>, IndexIssue>;
    /// Every stored key, in ascending order.
    fn for_each_key(&mut self, visit: &mut dyn FnMut(&str)) -> Result<(), IndexIssue>;
    /// The read window is over: release whatever it held (a lock, a transaction).
    fn finish_reads(&mut self) {}
    /// Apply `commit` atomically.
    fn commit(&mut self, commit: IndexCommit) -> Result<(), IndexIssue>;
    /// How many pairs are stored, when the backend knows cheaply.
    fn stored_pairs(&mut self) -> Option<u64> {
        None
    }
    /// Conditions the backend met that the run should name (a repair, a compaction).
    fn take_notes(&mut self) -> Vec<IndexIssue> {
        Vec::new()
    }
}

/// An index held in memory: the reference implementation the rules are tested against.
#[derive(Clone, Debug, Default)]
pub struct MemoryPairIndex {
    pub meta: Option<IndexMeta>,
    pub pairs: BTreeMap<String, StoredPair>,
    /// When set, the next commit fails with this issue and changes nothing.
    pub fail_next_commit: Option<IndexIssue>,
    /// How many commits were applied.
    pub commits: usize,
}

impl PairIndex for MemoryPairIndex {
    fn meta(&mut self) -> Result<Option<IndexMeta>, IndexIssue> {
        Ok(self.meta.clone())
    }

    fn get(&mut self, local_relative: &str) -> Result<Option<StoredPair>, IndexIssue> {
        Ok(self.pairs.get(local_relative).cloned())
    }

    fn for_each_key(&mut self, visit: &mut dyn FnMut(&str)) -> Result<(), IndexIssue> {
        for key in self.pairs.keys() {
            visit(key);
        }
        Ok(())
    }

    fn commit(&mut self, commit: IndexCommit) -> Result<(), IndexIssue> {
        if let Some(issue) = self.fail_next_commit.take() {
            return Err(issue);
        }
        if commit.clear {
            self.pairs.clear();
        }
        for key in commit.remove {
            self.pairs.remove(&key);
        }
        for (key, pair) in commit.upsert {
            self.pairs.insert(key, pair);
        }
        if let Some(meta) = commit.meta {
            self.meta = Some(meta);
        }
        self.commits += 1;
        Ok(())
    }

    fn stored_pairs(&mut self) -> Option<u64> {
        Some(self.pairs.len() as u64)
    }
}

/// How the index was used this run, for the result, the log and the dashboard.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IndexState {
    /// No `--pair-index`.
    #[default]
    Off,
    /// No usable evidence yet (new, retired or discarded): this run fills it.
    Cold,
    /// Evidence was available to serve.
    Warm,
    /// `--pair-index-refresh`: nothing served, everything rewritten.
    Refreshed,
    /// The index could not be used; every pair was verified live. See the issues.
    Unusable,
    /// Only local reads are skipped: the remote change time cannot be trusted.
    LocalOnly,
}

impl IndexState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Cold => "cold",
            Self::Warm => "warm",
            Self::Refreshed => "refreshed",
            Self::Unusable => "unusable",
            Self::LocalOnly => "local-only",
        }
    }
}

/// What the index did this run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexReport {
    pub state: IndexState,
    pub served: usize,
    pub verified_live: usize,
    /// Records found whose facts matched but which were racy, so verified live.
    pub racy: usize,
    /// Records found whose facts matched but which were expired, so verified live.
    pub expired: usize,
    pub canary_checked: usize,
    pub canary_inconclusive: usize,
    pub canary_mismatches: usize,
    /// The oldest verification time among served pairs (Unix seconds).
    pub oldest_evidence_epoch: Option<i64>,
    pub remote_signal: Option<RemoteSignal>,
    /// Pairs written and removed by the end-of-run commit.
    pub stored: usize,
    pub removed: usize,
    /// Pairs the index holds after the run, when the backend can say cheaply.
    pub entries: Option<u64>,
    pub issues: Vec<IndexIssue>,
}

impl IndexReport {
    /// The report for a run that was asked to use no index.
    pub fn off() -> Self {
        Self::default()
    }

    /// The report for an index that could not be used at all.
    pub fn unusable(issue: IndexIssue) -> Self {
        Self {
            state: IndexState::Unusable,
            issues: vec![issue],
            ..Self::default()
        }
    }
}

/// How a run uses the index.
#[derive(Clone, Copy, Debug)]
pub struct IndexOptions {
    pub max_age_seconds: i64,
    pub canary: usize,
    /// `--pair-index-refresh`: serve nothing, rewrite everything.
    pub refresh: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            max_age_seconds: DEFAULT_MAX_AGE_SECONDS,
            canary: crate::digest_evidence::DEFAULT_CANARY,
            refresh: false,
        }
    }
}

/// One run's use of an index: read in the plan pass, harvested from every live pass, written once.
pub struct IndexSession {
    backend: Box<dyn PairIndex>,
    binding: Binding,
    options: IndexOptions,
    /// Local clock at session start (Unix seconds).
    now: i64,
    seed: u64,
    signal: RemoteSignal,
    /// The pairs must be cleared at commit (retired, discarded, refreshed).
    clear: bool,
    /// Serving is off this run (refresh, link guard, a read failure), with no issue implied.
    serving: bool,
    /// The backend failed: nothing more is read or written this run.
    broken: bool,
    /// Pairs to write: the latest live proof per local key wins.
    upserts: BTreeMap<String, StoredPair>,
    /// Keys looked up whose facts no longer matched.
    invalidated: BTreeSet<String>,
    canary_at: i64,
    pub report: IndexReport,
}

impl IndexSession {
    /// Open `backend` for a run bound to `binding`.
    ///
    /// A `meta` that names another schema, epoch or binding retires the index: nothing is served
    /// and the pairs are cleared in this run's commit (`pair-index-retired`, info).
    pub fn open(
        mut backend: Box<dyn PairIndex>,
        binding: Binding,
        options: IndexOptions,
        now: SystemTime,
    ) -> Self {
        let now = unix_seconds(now);
        let mut report = IndexReport {
            state: IndexState::Warm,
            ..IndexReport::default()
        };
        let mut clear = false;
        let mut serving = true;
        let mut broken = false;
        let mut signal = RemoteSignal::Ctime;
        let mut canary_at = 0;
        match backend.meta() {
            Ok(None) => report.state = IndexState::Cold,
            Ok(Some(meta)) => {
                let mismatch = if meta.schema != SCHEMA {
                    Some(format!("schema {:?} is not {SCHEMA:?}", meta.schema))
                } else if meta.semantics_epoch != SEMANTICS_EPOCH {
                    Some(format!(
                        "semantics epoch {} is not {SEMANTICS_EPOCH}",
                        meta.semantics_epoch
                    ))
                } else {
                    binding_mismatch(&meta.binding, &binding)
                };
                if let Some(cause) = mismatch {
                    clear = true;
                    serving = false;
                    report.state = IndexState::Cold;
                    report.issues.push(IndexIssue::info(
                        "pair-index-retired",
                        &binding.profile,
                        cause,
                    ));
                } else {
                    signal = meta.remote_signal;
                    canary_at = meta.canary_at;
                }
            }
            Err(issue) => {
                broken = true;
                serving = false;
                report.state = IndexState::Unusable;
                report.issues.push(issue);
            }
        }
        if options.refresh && !broken {
            clear = true;
            serving = false;
            report.state = IndexState::Refreshed;
        }
        // The read window closes until the comparison set is known: the file is not held while
        // the run lists the NAS.
        backend.finish_reads();
        Self {
            backend,
            binding,
            options,
            now,
            seed: (now as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15),
            signal,
            clear,
            serving,
            broken,
            upserts: BTreeMap::new(),
            invalidated: BTreeSet::new(),
            canary_at,
            report,
        }
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// No serving this run, with the reason named (`pair-index-link-guard-unchecked`).
    pub fn suspend_serving(&mut self, issue: IndexIssue) {
        self.serving = false;
        self.report.issues.push(issue);
    }

    /// Decide whether the remote change time can be trusted this run, from the fresh listing.
    ///
    /// A comparison file listed without `ctime` means DSM does not report it: only the local
    /// read is skipped from now on (`pair-index-remote-ctime-unavailable`, info).
    pub fn observe_listing(&mut self, remote: &RemoteInventory) {
        if self.signal == RemoteSignal::Ctime
            && remote
                .entries
                .values()
                .any(|entry| entry.kind == EntryKind::File && entry.ctime_seconds.is_none())
        {
            self.signal = RemoteSignal::LocalOnly;
            self.report.issues.push(IndexIssue::info(
                "pair-index-remote-ctime-unavailable",
                &self.binding.remote,
                "the NAS listing carries no change time, so File Station's MD5 always runs and \
                 only the local read is skipped",
            ));
        }
    }

    /// The self-test of perf-design §2.8, against this run's own uploads.
    ///
    /// A file this run wrote must be listed afterwards with a change time no earlier than the NAS
    /// clock before execution minus the racy window, and later than its change time before the
    /// upload when it existed. A DSM whose `ctime` stood still for a write cannot reveal a
    /// replacement: only the local read is skipped from now on (`pair-index-remote-ctime-static`).
    pub fn self_test_uploads(
        &mut self,
        uploaded: &[String],
        before: &RemoteInventory,
        after: &RemoteInventory,
        nas_clock_before: Option<i64>,
    ) {
        if self.signal != RemoteSignal::Ctime {
            return;
        }
        for relative in uploaded {
            let Some(written) = after.entries.get(relative) else {
                continue;
            };
            let Some(ctime) = written.ctime_seconds else {
                continue;
            };
            let too_early =
                nas_clock_before.is_some_and(|clock| ctime < clock - RACY_WINDOW_SECONDS);
            let unmoved = before
                .entries
                .get(relative)
                .and_then(|previous| previous.ctime_seconds)
                .is_some_and(|previous| ctime <= previous);
            if too_early || unmoved {
                self.signal = RemoteSignal::LocalOnly;
                self.report.issues.push(IndexIssue::warning(
                    "pair-index-remote-ctime-static",
                    relative,
                    format!(
                        "the NAS listed {relative:?} with change time {ctime} after this run \
                         wrote it, which does not show the write; File Station's MD5 now always \
                         runs and only the local read is skipped"
                    ),
                ));
                return;
            }
        }
    }

    /// Take a whole-index discard: a canary disagreed. This run's live results are what the
    /// commit writes after the clear.
    fn discard(&mut self) {
        self.clear = true;
        self.serving = false;
    }

    /// Serve nothing from the stored evidence and clear it in this run's commit: the backend
    /// found damage it repaired, so none of it is trusted. The issue is the backend's to name.
    pub fn discard_evidence(&mut self) {
        self.discard();
        if self.report.state == IndexState::Warm {
            self.report.state = IndexState::Cold;
        }
    }

    fn mark_broken(&mut self, issue: IndexIssue) {
        self.broken = true;
        self.serving = false;
        self.report.state = IndexState::Unusable;
        self.report.issues.push(issue);
    }

    /// Whether `record` has expired at `now` for `path`.
    ///
    /// Rolling: `now - t <= max_age * (0.5 + h(path) / 2)`, so a static share is re-verified
    /// across the second half of the window rather than all at once. A record from the future
    /// has expired.
    pub fn expired(&self, path: &str, record: &PairRecord) -> bool {
        if record.verified_at > self.now {
            return true;
        }
        let fraction = path_fraction(path);
        let allowed = (self.options.max_age_seconds as f64 * (0.5 + fraction / 2.0)) as i64;
        self.now - record.verified_at > allowed
    }

    /// Record, for the end-of-run commit, every pair in `live` (remote relatives) whose digests
    /// were both computed live this pass and agree.
    ///
    /// `read_started` is the local clock when the pass's local reads began: a local change time
    /// within the racy window of it, or a remote one within the window of the `MD5.start` clock
    /// (or no NAS clock at all), makes the record racy.
    pub fn harvest(
        &mut self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        live: &BTreeSet<String>,
        spellings: &BTreeMap<String, String>,
        read_started: SystemTime,
    ) {
        if self.broken {
            return;
        }
        let started_ns = unix_nanos(read_started);
        let started = started_ns.div_euclid(1_000_000_000);
        for remote_relative in live {
            let local_relative = spellings
                .get(remote_relative)
                .map_or(remote_relative.as_str(), String::as_str);
            let (Some(local_entry), Some(remote_entry)) = (
                local.entries.get(local_relative),
                remote.entries.get(remote_relative),
            ) else {
                continue;
            };
            let (Some(local_digest), Some(remote_digest)) =
                (local_entry.content_md5, remote_entry.content_md5)
            else {
                continue;
            };
            if local_digest.as_bytes() != remote_digest.as_bytes()
                || local_entry.kind != EntryKind::File
                || remote_entry.kind != EntryKind::File
                || !local_entry.identity.is_known()
                || !local_entry.times.is_known()
            {
                continue;
            }
            let local_racy = local_entry.times.ctime_ns > started_ns - RACY_WINDOW_NS;
            let remote_racy = match (remote_entry.ctime_seconds, remote_entry.digest_nas_time) {
                (Some(ctime), Some(clock)) => ctime > clock - RACY_WINDOW_SECONDS,
                (Some(_), None) => true,
                (None, _) => false,
            };
            let record = PairRecord {
                size: local_entry.size,
                local_mtime_ns: local_entry.times.mtime_ns,
                local_ctime_ns: local_entry.times.ctime_ns,
                device: local_entry.identity.device,
                inode: local_entry.identity.inode,
                remote_mtime: remote_entry.mtime_seconds,
                remote_ctime: remote_entry.ctime_seconds,
                remote_crtime: remote_entry.crtime_seconds,
                md5: *local_digest.as_bytes(),
                verified_at: started,
                verified_at_nas: remote_entry.digest_nas_time,
                racy: local_racy || remote_racy,
            };
            self.upserts.insert(
                local_relative.to_owned(),
                StoredPair {
                    record,
                    remote_spelling: (local_relative != remote_relative)
                        .then(|| remote_relative.clone()),
                },
            );
        }
    }

    /// Write this run's evidence in one transaction and report what it did.
    ///
    /// Removes every key this run touched (unless it was re-proven live afterwards), every key
    /// whose facts no longer matched, and, for an unscoped run, every key that is no longer a
    /// local file. `skip` (a breaker trip, a failed run) writes nothing and keeps the old
    /// evidence exactly as it was.
    pub fn commit(
        mut self,
        local: Option<&LocalInventory>,
        touched: Option<&TouchedPaths>,
        unscoped: bool,
    ) -> IndexReport {
        self.report.remote_signal = Some(self.signal);
        if self.signal == RemoteSignal::LocalOnly && self.report.state == IndexState::Warm {
            self.report.state = IndexState::LocalOnly;
        }
        if self.broken {
            return self.report;
        }
        let mut remove = Vec::new();
        if !self.clear {
            let upserts = &self.upserts;
            let invalidated = &self.invalidated;
            let mut visit = |key: &str| {
                if upserts.contains_key(key) {
                    return;
                }
                let stale = unscoped
                    && local.is_some_and(|local| {
                        local
                            .entries
                            .get(key)
                            .is_none_or(|entry| entry.kind != EntryKind::File)
                    });
                if stale
                    || invalidated.contains(key)
                    || touched.is_some_and(|touched| touched.touches(key))
                {
                    remove.push(key.to_owned());
                }
            };
            if let Err(issue) = self.backend.for_each_key(&mut visit) {
                self.mark_broken(issue);
                return self.report;
            }
        }
        self.backend.finish_reads();
        let stored = self.upserts.len();
        let removed = remove.len();
        let commit = IndexCommit {
            clear: self.clear,
            remove,
            upsert: std::mem::take(&mut self.upserts).into_iter().collect(),
            meta: Some(IndexMeta {
                schema: SCHEMA.to_owned(),
                semantics_epoch: SEMANTICS_EPOCH,
                binding: self.binding.clone(),
                remote_signal: self.signal,
                canary_at: self.canary_at,
            }),
        };
        match self.backend.commit(commit) {
            Ok(()) => {
                self.report.stored = stored;
                self.report.removed = removed;
                self.report.entries = self.backend.stored_pairs();
            }
            Err(issue) => {
                self.report.issues.push(issue);
            }
        }
        self.report.issues.extend(self.backend.take_notes());
        self.report
    }

    /// The run did not finish its digest pass (a breaker trip, a failed run): nothing is written,
    /// and the evidence stays exactly as it was.
    pub fn abandon(mut self) -> IndexReport {
        self.backend.finish_reads();
        self.report.entries = self.backend.stored_pairs();
        self.report.remote_signal = Some(self.signal);
        self.report.issues.extend(self.backend.take_notes());
        self.report
    }

    /// Facts check for one stored pair against this run's fresh entries.
    fn facts_match(
        &self,
        stored: &StoredPair,
        local: &LocalEntry,
        remote: &RemoteEntry,
        remote_relative: &str,
        local_relative: &str,
    ) -> bool {
        let record = &stored.record;
        let spelling_matches = match &stored.remote_spelling {
            Some(spelling) => spelling == remote_relative,
            None => local_relative == remote_relative,
        };
        let local_matches = local.kind == EntryKind::File
            && local.identity.is_known()
            && local.times.is_known()
            && local.size == record.size
            && local.times.mtime_ns == record.local_mtime_ns
            && local.times.ctime_ns == record.local_ctime_ns
            && local.identity.device == record.device
            && local.identity.inode == record.inode;
        let remote_matches = remote.kind == EntryKind::File
            && remote.mount_point_type.is_none()
            && remote.size == record.size
            && remote.mtime_seconds == record.remote_mtime
            && match self.signal {
                // The remote side is vouched for only on an exact change-time match, and a
                // creation time when one was recorded.
                RemoteSignal::Ctime => {
                    record.remote_ctime.is_some()
                        && remote.ctime_seconds == record.remote_ctime
                        && (record.remote_crtime.is_none()
                            || remote.crtime_seconds == record.remote_crtime)
                }
                // Read live anyway; size and mtime only decide whether the record still names
                // this pair.
                RemoteSignal::LocalOnly => true,
            };
        spelling_matches && local_matches && remote_matches
    }
}

impl EvidenceSource for IndexSession {
    fn vouch(
        &mut self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        comparison: &BTreeSet<String>,
        strong: &BTreeSet<String>,
        spellings: &BTreeMap<String, String>,
    ) -> Vouched {
        if !self.serving || self.broken {
            return Vouched::default();
        }
        let mut eligible = Vec::new();
        let mut oldest: Option<i64> = None;
        for remote_relative in comparison {
            if strong.contains(remote_relative) {
                continue;
            }
            let local_relative = spellings
                .get(remote_relative)
                .map_or(remote_relative.as_str(), String::as_str);
            let (Some(local_entry), Some(remote_entry)) = (
                local.entries.get(local_relative),
                remote.entries.get(remote_relative),
            ) else {
                continue;
            };
            let stored = match self.backend.get(local_relative) {
                Ok(Some(stored)) => stored,
                Ok(None) => continue,
                Err(issue) => {
                    // A read that fails part-way serves nothing at all: what was gathered so far
                    // is dropped with the rest.
                    self.mark_broken(issue);
                    return Vouched::default();
                }
            };
            if !self.facts_match(
                &stored,
                local_entry,
                remote_entry,
                remote_relative,
                local_relative,
            ) {
                self.invalidated.insert(local_relative.to_owned());
                continue;
            }
            if stored.record.racy {
                self.report.racy += 1;
                continue;
            }
            if self.expired(local_relative, &stored.record) {
                self.report.expired += 1;
                continue;
            }
            oldest = Some(oldest.map_or(stored.record.verified_at, |current| {
                current.min(stored.record.verified_at)
            }));
            eligible.push(VouchedPair {
                local: local_relative.to_owned(),
                remote: remote_relative.clone(),
                md5: stored.record.md5,
                remote_too: self.signal == RemoteSignal::Ctime,
            });
        }
        self.backend.finish_reads();
        let vouched = split_canaries(eligible, self.options.canary, self.seed);
        if !vouched.canaries.is_empty() {
            self.canary_at = self.now;
        }
        self.report.oldest_evidence_epoch = oldest;
        vouched
    }

    fn observe_live(
        &mut self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        live: &BTreeSet<String>,
        spellings: &BTreeMap<String, String>,
        read_started: SystemTime,
    ) {
        self.harvest(local, remote, live, spellings, read_started);
    }

    fn conclude(&mut self, evidence: &EvidenceReport) {
        self.report.served = evidence.served;
        self.report.verified_live = evidence.verified_live;
        self.report.canary_checked = evidence.canary_checked;
        self.report.canary_inconclusive = evidence.canary_inconclusive;
        if let Some(mismatch) = &evidence.canary_mismatch {
            self.report.canary_mismatches += 1;
            self.report.issues.push(IndexIssue::warning(
                "pair-index-canary-mismatch",
                &mismatch.path,
                format!(
                    "read live as {} locally and {} on the NAS where the index held {}; the \
                     index was discarded and every pair verified live",
                    mismatch.local,
                    mismatch.remote.as_deref().unwrap_or("(not re-read)"),
                    mismatch.vouched
                ),
            ));
            self.discard();
            if self.report.state == IndexState::Warm {
                self.report.state = IndexState::Cold;
            }
        }
    }
}

/// Which bound field differs, named.
fn binding_mismatch(stored: &Binding, current: &Binding) -> Option<String> {
    let differs = |field: &str, was: &str, now: &str| {
        Some(format!(
            "bound to {field} {was:?}, this run uses {now:?}; retired"
        ))
    };
    if stored.profile != current.profile {
        return differs("profile", &stored.profile, &current.profile);
    }
    if stored.source != current.source {
        return differs("source", &stored.source, &current.source);
    }
    if stored.source_device != current.source_device {
        return differs(
            "source device",
            &stored.source_device.to_string(),
            &current.source_device.to_string(),
        );
    }
    if stored.remote != current.remote {
        return differs("remote", &stored.remote, &current.remote);
    }
    if stored.server != current.server {
        return differs("server", &stored.server, &current.server);
    }
    None
}

/// A stable value in `[0, 1)` for `path` (FNV-1a), for rolling expiry.
fn path_fraction(path: &str) -> f64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    (hash >> 11) as f64 / (1_u64 << 53) as f64
}

fn unix_seconds(time: SystemTime) -> i64 {
    unix_nanos(time).div_euclid(1_000_000_000)
}

fn unix_nanos(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

/// The paths `uploaded` and copied by a plan, for the remote change-time self-test.
pub fn written_paths(plan: &crate::plan::SyncPlan) -> Vec<String> {
    let mut written: HashSet<String> = plan
        .uploads
        .iter()
        .map(|action| action.local.relative.clone())
        .collect();
    written.extend(plan.copies.iter().map(|action| action.to_relative.clone()));
    let mut written: Vec<String> = written.into_iter().collect();
    written.sort();
    written
}

#[cfg(test)]
mod tests;
