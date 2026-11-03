//! Comparison digests a run may take from evidence instead of reading both sides again.
//!
//! Content mode settles every same-size pair with two MD5s: one read of the local file and one
//! File Station `MD5` task that makes the NAS read its copy. In the steady state that is the whole
//! tree on both sides, every run, and twice for a run that executed anything (once to plan, once
//! to reconcile). An [`EvidenceSource`] may vouch for a pair instead, when it holds a digest that
//! both sides were proven to agree on and every fact that could reveal a change since is
//! unchanged. Two sources exist:
//!
//! - [`PlanCarry`]: within one run, reconciliation takes the plan pass's own digests for the pairs
//!   the run did not touch (SC-3, perf-design §3).
//! - the verified-pair index ([`crate::pair_index`]): across runs (perf-design §2).
//!
//! Whatever the source, the rules here are the same, and they are what keeps the server's MD5 the
//! authority for content equality:
//!
//! - A vouched digest is only ever MD5-only ([`ContentMd5::from_bytes`]), so
//!   `has_full_proof() == false` and no mutation guard can use it: server copy and the deletion
//!   guard need full proof, and the deletion guard's candidates have no local counterpart, so they
//!   are never a pair at all. A vouched pair is equal, so it can only ever suppress an upload.
//! - A sample of the pairs a source could vouch for (the canary) is verified live every run. A
//!   live digest that disagrees with the vouched one while every fact matched means the source is
//!   wrong in a way facts cannot show; every vouched pair is then re-verified live in the same
//!   run, and the disagreement is reported by name. The run's own verdict then rests on live
//!   evidence only, so it is never failed or made partial by the source.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::api::{RemoteEntry, RemoteInventory};
use crate::integrity::ContentMd5;
use crate::local::{EntryKind, LocalEntry, LocalInventory};
use crate::plan::SyncPlan;
use crate::sync::DeferredPath;

/// Pairs re-verified live per run out of those a source could vouch for.
pub const DEFAULT_CANARY: usize = 25;

/// How many hex digits of a digest a report names: enough to tell two digests apart by eye,
/// never the whole value.
const DIGEST_PREFIX_HEX: usize = 12;

/// One comparison pair a source vouches for, with the digest it vouches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VouchedPair {
    /// The local relative of the pair.
    pub local: String,
    /// The remote relative of the pair (the comparison set's key).
    pub remote: String,
    /// The MD5 both sides were proven to share.
    pub md5: [u8; 16],
    /// False when only the local read may be skipped: the remote side has no change signal the
    /// source can trust, so File Station's MD5 still runs for it (perf-design §2.8).
    pub remote_too: bool,
}

/// What a source vouches for in one pass: pairs whose digests it supplies, and pairs it could
/// have supplied that are verified live instead, to check it.
#[derive(Debug, Default)]
pub struct Vouched {
    pub served: Vec<VouchedPair>,
    pub canaries: Vec<VouchedPair>,
}

/// A store or carry of verified pairs that can stand in for live digests.
pub trait EvidenceSource {
    /// Choose the pairs to vouch for among `comparison` (remote relatives).
    ///
    /// `strong` pairs are fetched at full strength for a mutation guard and must not be vouched.
    /// `spellings` maps a remote relative to its local relative where the two differ.
    fn vouch(
        &mut self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        comparison: &BTreeSet<String>,
        strong: &BTreeSet<String>,
        spellings: &BTreeMap<String, String>,
    ) -> Vouched;

    /// The pairs in `live` (remote relatives) had both digests computed live by this pass, from
    /// local reads that began at `read_started`. A store harvests its evidence here.
    fn observe_live(
        &mut self,
        _local: &LocalInventory,
        _remote: &RemoteInventory,
        _live: &BTreeSet<String>,
        _spellings: &BTreeMap<String, String>,
        _read_started: std::time::SystemTime,
    ) {
    }

    /// What the pass did with this source's evidence, canary verdict included.
    fn conclude(&mut self, _report: &EvidenceReport) {}
}

/// Reconciliation's evidence: the SC-3 carry (unless `--reconcile full`), plus a store that
/// harvests the pass's live proofs without serving anything (the verified-pair index).
pub struct ReconcileEvidence<'run, 'plan> {
    pub carry: Option<PlanCarry<'plan>>,
    pub store: Option<&'run mut dyn EvidenceSource>,
}

impl EvidenceSource for ReconcileEvidence<'_, '_> {
    fn vouch(
        &mut self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        comparison: &BTreeSet<String>,
        strong: &BTreeSet<String>,
        spellings: &BTreeMap<String, String>,
    ) -> Vouched {
        match &mut self.carry {
            Some(carry) => carry.vouch(local, remote, comparison, strong, spellings),
            None => Vouched::default(),
        }
    }

    fn observe_live(
        &mut self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        live: &BTreeSet<String>,
        spellings: &BTreeMap<String, String>,
        read_started: std::time::SystemTime,
    ) {
        if let Some(store) = self.store.as_deref_mut() {
            store.observe_live(local, remote, live, spellings, read_started);
        }
    }
}

/// A canary pair whose live digest disagreed with the vouched one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanaryMismatch {
    /// The local relative of the pair.
    pub path: String,
    /// Hex prefix of the vouched digest.
    pub vouched: String,
    /// Hex prefix of the live local digest.
    pub local: String,
    /// Hex prefix of the live remote digest, when it was computed live.
    pub remote: Option<String>,
}

/// What evidence did for one digest pass, for the result, the log and the dashboard.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EvidenceReport {
    /// Pairs whose digests came from evidence and were not read on either vouched side.
    pub served: usize,
    /// Comparison pairs whose remote digest was computed live by this pass.
    pub verified_live: usize,
    /// Canary pairs verified live with a conclusive answer.
    pub canary_checked: usize,
    /// Canary pairs that could not be verified live (deferred, vanished, unreadable).
    pub canary_inconclusive: usize,
    /// The first canary disagreement, when there was one.
    pub canary_mismatch: Option<CanaryMismatch>,
    /// True when a canary disagreement made this pass re-verify every vouched pair live.
    pub fell_back: bool,
}

/// Fill the digests `served` vouches for, and take each fully vouched pair out of `comparison`
/// so neither side is read.
///
/// A pair only the local side of which is vouched keeps its remote relative in `comparison`, so
/// File Station's MD5 still runs for it.
pub fn apply_vouched(
    local: &mut LocalInventory,
    remote: &mut RemoteInventory,
    comparison: &mut BTreeSet<String>,
    served: &[VouchedPair],
) {
    for pair in served {
        let digest = ContentMd5::from_bytes(pair.md5);
        if let Some(entry) = local.entries.get_mut(&pair.local) {
            entry.content_md5 = Some(digest);
        }
        if pair.remote_too {
            if let Some(entry) = remote.entries.get_mut(&pair.remote) {
                entry.content_md5 = Some(digest);
            }
            comparison.remove(&pair.remote);
        }
    }
}

/// Undo [`apply_vouched`]: clear every vouched digest, returning the remote relatives whose MD5
/// must now be computed live.
pub fn undo_vouched(
    local: &mut LocalInventory,
    remote: &mut RemoteInventory,
    served: &[VouchedPair],
) -> BTreeSet<String> {
    let mut redo = BTreeSet::new();
    for pair in served {
        if let Some(entry) = local.entries.get_mut(&pair.local) {
            entry.content_md5 = None;
        }
        if pair.remote_too
            && let Some(entry) = remote.entries.get_mut(&pair.remote)
        {
            entry.content_md5 = None;
            redo.insert(pair.remote.clone());
        }
    }
    redo
}

/// Compare each canary's live digests with the digest the source vouched for it.
///
/// A canary whose live local or remote digest is missing (the file was deferred, vanished or
/// could not be read) is inconclusive and counted, never a mismatch.
pub fn check_canaries(
    local: &LocalInventory,
    remote: &RemoteInventory,
    canaries: &[VouchedPair],
    report: &mut EvidenceReport,
) {
    for pair in canaries {
        let live_local = local
            .entries
            .get(&pair.local)
            .and_then(|entry| entry.content_md5);
        let live_remote = remote
            .entries
            .get(&pair.remote)
            .and_then(|entry| entry.content_md5);
        let (Some(live_local), Some(live_remote)) = (live_local, live_remote) else {
            report.canary_inconclusive += 1;
            continue;
        };
        report.canary_checked += 1;
        let local_agrees = *live_local.as_bytes() == pair.md5;
        let remote_agrees = *live_remote.as_bytes() == pair.md5;
        if (!local_agrees || !remote_agrees) && report.canary_mismatch.is_none() {
            report.canary_mismatch = Some(CanaryMismatch {
                path: pair.local.clone(),
                vouched: hex_prefix(&pair.md5),
                local: hex_prefix(live_local.as_bytes()),
                remote: pair.remote_too.then(|| hex_prefix(live_remote.as_bytes())),
            });
        }
    }
}

fn hex_prefix(bytes: &[u8; 16]) -> String {
    let mut text = String::with_capacity(DIGEST_PREFIX_HEX);
    for byte in bytes {
        if text.len() >= DIGEST_PREFIX_HEX {
            break;
        }
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// Split `eligible` into served pairs and up to `canary` canaries, taken at a stride with an
/// offset from `seed`, so successive runs check different pairs and every pair is drawn
/// eventually.
pub fn split_canaries(mut eligible: Vec<VouchedPair>, canary: usize, seed: u64) -> Vouched {
    if canary == 0 || eligible.is_empty() {
        return Vouched {
            served: eligible,
            canaries: Vec::new(),
        };
    }
    if eligible.len() <= canary {
        return Vouched {
            served: Vec::new(),
            canaries: eligible,
        };
    }
    let stride = eligible.len() / canary;
    let offset = usize::try_from(seed % stride as u64).unwrap_or(0);
    let mut chosen = HashSet::with_capacity(canary);
    for step in 0..canary {
        chosen.insert(offset + step * stride);
    }
    let mut canaries = Vec::with_capacity(canary);
    let mut index = 0;
    eligible.retain(|pair| {
        let keep = !chosen.contains(&index);
        if !keep {
            canaries.push(pair.clone());
        }
        index += 1;
        keep
    });
    Vouched {
        served: eligible,
        canaries,
    }
}

/// Whether a fresh local observation of a file has exactly the facts of an earlier one, by every
/// signal that can reveal a write: size, millisecond and nanosecond mtime, ctime, device and
/// inode.
///
/// Unknown identity or ctime (Windows, a filesystem without them) never matches: two unknowns
/// comparing equal establish nothing (perf-design §2.9, L7).
pub fn local_facts_match(fresh: &LocalEntry, earlier: &LocalEntry) -> bool {
    fresh.kind == EntryKind::File
        && earlier.kind == EntryKind::File
        && fresh.identity.is_known()
        && fresh.times.is_known()
        && fresh.size == earlier.size
        && fresh.mtime_ms == earlier.mtime_ms
        && fresh.identity == earlier.identity
        && fresh.times == earlier.times
}

/// Whether a fresh remote listing of a file has exactly the facts of an earlier one: path, size,
/// mtime, and the change and creation times as DSM reported them.
pub fn remote_facts_match(fresh: &RemoteEntry, earlier: &RemoteEntry) -> bool {
    fresh.kind == EntryKind::File
        && earlier.kind == EntryKind::File
        && fresh.remote_path == earlier.remote_path
        && fresh.size == earlier.size
        && fresh.mtime_seconds == earlier.mtime_seconds
        && fresh.ctime_seconds == earlier.ctime_seconds
        && fresh.crtime_seconds == earlier.crtime_seconds
        && fresh.mount_point_type.is_none()
        && earlier.mount_point_type.is_none()
}

/// Every path a run's execution touched, folded so that both spellings of a case or
/// normalisation rename are covered, with everything beneath each of them.
#[derive(Clone, Debug, Default)]
pub struct TouchedPaths {
    folded: HashSet<String>,
}

impl TouchedPaths {
    /// The paths `plan` wrote, created, copied from, deleted or renamed, and every path the run
    /// deferred.
    ///
    /// Subtree semantics are deliberate: a deleted or renamed folder changes every path beneath
    /// it, and over-touching only costs a live verification.
    pub fn from_run(plan: &SyncPlan, deferred: &[DeferredPath]) -> Self {
        let mut touched = Self::default();
        for action in &plan.uploads {
            touched.insert(&action.local.relative);
        }
        for action in &plan.copies {
            touched.insert(&action.to_relative);
            touched.insert(&action.from_relative);
        }
        for action in &plan.creates {
            touched.insert(&action.relative);
        }
        for action in plan.pre_deletes.iter().chain(&plan.post_deletes) {
            touched.insert(&action.relative);
            touched.insert(&action.local_relative);
        }
        for action in &plan.renames {
            touched.insert(&action.relative);
            touched.insert(&action.from_relative);
        }
        for record in deferred {
            touched.insert(&record.relative);
        }
        touched
    }

    pub fn insert(&mut self, relative: &str) {
        self.folded.insert(crate::path::portable_fold(relative));
    }

    pub fn len(&self) -> usize {
        self.folded.len()
    }

    pub fn is_empty(&self) -> bool {
        self.folded.is_empty()
    }

    /// Whether `relative`, or a folder above it, was touched.
    pub fn touches(&self, relative: &str) -> bool {
        if self.folded.is_empty() {
            return false;
        }
        // The fold of a path's prefix is the same prefix of its fold (see `portable_fold`).
        let folded = crate::path::portable_fold(relative);
        let mut end = folded.len();
        loop {
            if self.folded.contains(&folded[..end]) {
                return true;
            }
            match folded[..end].rfind('/') {
                Some(slash) => end = slash,
                None => return false,
            }
        }
    }
}

/// The plan pass's own evidence, carried into the same run's reconciliation (SC-3).
///
/// The fresh reconciliation walk and listing are kept in full: they are what proves nothing else
/// moved. A pair is carried when the run did not touch it, both sides' facts (ctimes included)
/// equal the plan-time facts, and the plan pass proved both sides equal. Everything else is
/// verified live: touched pairs, pairs whose facts moved, and the canary.
pub struct PlanCarry<'plan> {
    pub local: &'plan LocalInventory,
    pub remote: &'plan RemoteInventory,
    pub touched: TouchedPaths,
    pub canary: usize,
    pub seed: u64,
}

impl EvidenceSource for PlanCarry<'_> {
    fn vouch(
        &mut self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        comparison: &BTreeSet<String>,
        strong: &BTreeSet<String>,
        spellings: &BTreeMap<String, String>,
    ) -> Vouched {
        let mut eligible = Vec::new();
        for remote_relative in comparison {
            if strong.contains(remote_relative) {
                continue;
            }
            let local_relative = spellings
                .get(remote_relative)
                .map_or(remote_relative.as_str(), String::as_str);
            if self.touched.touches(local_relative) || self.touched.touches(remote_relative) {
                continue;
            }
            let (Some(fresh_local), Some(plan_local)) = (
                local.entries.get(local_relative),
                self.local.entries.get(local_relative),
            ) else {
                continue;
            };
            let (Some(fresh_remote), Some(plan_remote)) = (
                remote.entries.get(remote_relative),
                self.remote.entries.get(remote_relative),
            ) else {
                continue;
            };
            if !local_facts_match(fresh_local, plan_local)
                || !remote_facts_match(fresh_remote, plan_remote)
            {
                continue;
            }
            let (Some(local_digest), Some(remote_digest)) =
                (plan_local.content_md5, plan_remote.content_md5)
            else {
                continue;
            };
            if local_digest.as_bytes() != remote_digest.as_bytes() {
                continue;
            }
            eligible.push(VouchedPair {
                local: local_relative.to_owned(),
                remote: remote_relative.clone(),
                md5: *local_digest.as_bytes(),
                remote_too: true,
            });
        }
        split_canaries(eligible, self.canary, self.seed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::{ChangeTimes, FileIdentity};
    use std::path::PathBuf;

    fn local_entry(relative: &str, size: u64, ctime_ns: i64, md5: Option<[u8; 16]>) -> LocalEntry {
        LocalEntry {
            relative: relative.to_owned(),
            full_path: PathBuf::from(relative),
            kind: EntryKind::File,
            size,
            mtime_ms: 1_000,
            identity: FileIdentity {
                device: 1,
                inode: 7,
            },
            times: ChangeTimes {
                mtime_ns: 1_000_000_000,
                ctime_ns,
            },
            content_md5: md5.map(ContentMd5::from_bytes),
        }
    }

    fn remote_entry(relative: &str, size: u64, ctime: i64, md5: Option<[u8; 16]>) -> RemoteEntry {
        RemoteEntry {
            relative: relative.to_owned(),
            remote_path: format!("/share/{relative}"),
            kind: EntryKind::File,
            size,
            mtime_seconds: 1,
            ctime_seconds: Some(ctime),
            crtime_seconds: Some(1),
            digest_nas_time: None,
            mount_point_type: None,
            content_md5: md5.map(ContentMd5::from_bytes),
        }
    }

    fn inventories(
        entries: &[(&str, i64, i64, Option<[u8; 16]>)],
    ) -> (LocalInventory, RemoteInventory) {
        let mut local = LocalInventory {
            root: PathBuf::from("/src"),
            entries: BTreeMap::new(),
            skipped: Vec::new(),
        };
        let mut remote = RemoteInventory {
            root_exists: true,
            entries: BTreeMap::new(),
        };
        for (relative, local_ctime, remote_ctime, md5) in entries {
            local.entries.insert(
                (*relative).to_owned(),
                local_entry(relative, 5, *local_ctime, *md5),
            );
            remote.entries.insert(
                (*relative).to_owned(),
                remote_entry(relative, 5, *remote_ctime, *md5),
            );
        }
        (local, remote)
    }

    fn clear_digests(local: &mut LocalInventory, remote: &mut RemoteInventory) {
        for entry in local.entries.values_mut() {
            entry.content_md5 = None;
        }
        for entry in remote.entries.values_mut() {
            entry.content_md5 = None;
        }
    }

    fn names(pairs: &[VouchedPair]) -> Vec<&str> {
        pairs.iter().map(|pair| pair.local.as_str()).collect()
    }

    const A: [u8; 16] = [0xaa; 16];
    const B: [u8; 16] = [0xbb; 16];

    #[test]
    fn carry_takes_only_untouched_pairs_whose_facts_and_digests_held() {
        let (plan_local, plan_remote) = inventories(&[
            ("same", 10, 20, Some(A)),
            ("uploaded", 10, 20, Some(A)),
            ("local-ctime-moved", 10, 20, Some(A)),
            ("remote-ctime-moved", 10, 20, Some(A)),
            ("differed", 10, 20, None),
            ("dir/beneath-deleted", 10, 20, Some(A)),
        ]);
        let (mut fresh_local, mut fresh_remote) = inventories(&[
            ("same", 10, 20, None),
            ("uploaded", 10, 20, None),
            ("local-ctime-moved", 11, 20, None),
            ("remote-ctime-moved", 10, 21, None),
            ("differed", 10, 20, None),
            ("dir/beneath-deleted", 10, 20, None),
        ]);
        clear_digests(&mut fresh_local, &mut fresh_remote);
        let mut touched = TouchedPaths::default();
        touched.insert("uploaded");
        touched.insert("DIR");
        let mut carry = PlanCarry {
            local: &plan_local,
            remote: &plan_remote,
            touched,
            canary: 0,
            seed: 0,
        };
        let comparison: BTreeSet<String> = fresh_remote
            .entries
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let vouched = carry.vouch(
            &fresh_local,
            &fresh_remote,
            &comparison,
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert_eq!(names(&vouched.served), vec!["same"]);
        assert!(vouched.canaries.is_empty());
    }

    #[test]
    fn a_pair_whose_plan_digests_disagreed_is_never_carried() {
        let (mut plan_local, plan_remote) = inventories(&[("x", 10, 20, Some(A))]);
        plan_local.entries.get_mut("x").unwrap().content_md5 = Some(ContentMd5::from_bytes(B));
        let (fresh_local, fresh_remote) = inventories(&[("x", 10, 20, None)]);
        let mut carry = PlanCarry {
            local: &plan_local,
            remote: &plan_remote,
            touched: TouchedPaths::default(),
            canary: 0,
            seed: 0,
        };
        let comparison = BTreeSet::from(["x".to_owned()]);
        let vouched = carry.vouch(
            &fresh_local,
            &fresh_remote,
            &comparison,
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert!(vouched.served.is_empty());
    }

    #[test]
    fn unknown_local_facts_are_never_carried() {
        let (mut plan_local, plan_remote) = inventories(&[("x", 10, 20, Some(A))]);
        let (mut fresh_local, fresh_remote) = inventories(&[("x", 10, 20, None)]);
        for inventory in [&mut plan_local, &mut fresh_local] {
            inventory.entries.get_mut("x").unwrap().times = ChangeTimes::default();
        }
        let mut carry = PlanCarry {
            local: &plan_local,
            remote: &plan_remote,
            touched: TouchedPaths::default(),
            canary: 0,
            seed: 0,
        };
        let comparison = BTreeSet::from(["x".to_owned()]);
        assert!(
            carry
                .vouch(
                    &fresh_local,
                    &fresh_remote,
                    &comparison,
                    &BTreeSet::new(),
                    &BTreeMap::new()
                )
                .served
                .is_empty()
        );
    }

    #[test]
    fn strong_pairs_are_never_vouched() {
        let (plan_local, plan_remote) = inventories(&[("x", 10, 20, Some(A))]);
        let (fresh_local, fresh_remote) = inventories(&[("x", 10, 20, None)]);
        let mut carry = PlanCarry {
            local: &plan_local,
            remote: &plan_remote,
            touched: TouchedPaths::default(),
            canary: 0,
            seed: 0,
        };
        let comparison = BTreeSet::from(["x".to_owned()]);
        let strong = comparison.clone();
        assert!(
            carry
                .vouch(
                    &fresh_local,
                    &fresh_remote,
                    &comparison,
                    &strong,
                    &BTreeMap::new()
                )
                .served
                .is_empty()
        );
    }

    #[test]
    fn touched_covers_both_spellings_and_everything_beneath() {
        let mut touched = TouchedPaths::default();
        touched.insert("Docs/Report.TXT");
        touched.insert("old");
        assert!(touched.touches("docs/report.txt"));
        assert!(touched.touches("old"));
        assert!(touched.touches("OLD/deep/file"));
        assert!(!touched.touches("older"));
        assert!(!touched.touches("docs"));
        assert!(!touched.touches("docs/report.txt.bak"));
    }

    #[test]
    fn canaries_are_drawn_at_a_stride_and_vary_with_the_seed() {
        let pairs: Vec<VouchedPair> = (0..100)
            .map(|index| VouchedPair {
                local: format!("f{index:03}"),
                remote: format!("f{index:03}"),
                md5: A,
                remote_too: true,
            })
            .collect();
        let first = split_canaries(pairs.clone(), 25, 0);
        assert_eq!(first.canaries.len(), 25);
        assert_eq!(first.served.len(), 75);
        let second = split_canaries(pairs.clone(), 25, 3);
        assert_ne!(first.canaries, second.canaries);
        let all = split_canaries(pairs[..10].to_vec(), 25, 9);
        assert_eq!(all.canaries.len(), 10);
        assert!(all.served.is_empty());
        let none = split_canaries(pairs, 0, 9);
        assert!(none.canaries.is_empty());
    }

    #[test]
    fn a_canary_that_disagrees_is_reported_with_digest_prefixes_and_a_missing_one_is_inconclusive()
    {
        let (local, remote) = inventories(&[("a", 1, 1, Some(A)), ("b", 1, 1, Some(B))]);
        let canaries = vec![
            VouchedPair {
                local: "a".to_owned(),
                remote: "a".to_owned(),
                md5: A,
                remote_too: true,
            },
            VouchedPair {
                local: "b".to_owned(),
                remote: "b".to_owned(),
                md5: A,
                remote_too: true,
            },
            VouchedPair {
                local: "gone".to_owned(),
                remote: "gone".to_owned(),
                md5: A,
                remote_too: true,
            },
        ];
        let mut report = EvidenceReport::default();
        check_canaries(&local, &remote, &canaries, &mut report);
        assert_eq!(report.canary_checked, 2);
        assert_eq!(report.canary_inconclusive, 1);
        let mismatch = report.canary_mismatch.expect("b disagrees");
        assert_eq!(mismatch.path, "b");
        assert_eq!(mismatch.vouched, "aaaaaaaaaaaa");
        assert_eq!(mismatch.local, "bbbbbbbbbbbb");
        assert_eq!(mismatch.remote.as_deref(), Some("bbbbbbbbbbbb"));
    }

    #[test]
    fn applying_and_undoing_vouched_digests_restores_live_work() {
        let (mut local, mut remote) = inventories(&[("a", 1, 1, None), ("b", 1, 1, None)]);
        let mut comparison: BTreeSet<String> = ["a".to_owned(), "b".to_owned()].into();
        let served = vec![
            VouchedPair {
                local: "a".to_owned(),
                remote: "a".to_owned(),
                md5: A,
                remote_too: true,
            },
            VouchedPair {
                local: "b".to_owned(),
                remote: "b".to_owned(),
                md5: B,
                remote_too: false,
            },
        ];
        apply_vouched(&mut local, &mut remote, &mut comparison, &served);
        assert_eq!(comparison, BTreeSet::from(["b".to_owned()]));
        let served_digest = local.entries["a"].content_md5.unwrap();
        assert!(!served_digest.has_full_proof(), "the type barrier");
        assert!(remote.entries["b"].content_md5.is_none());
        let redo = undo_vouched(&mut local, &mut remote, &served);
        assert_eq!(redo, BTreeSet::from(["a".to_owned()]));
        assert!(
            local
                .entries
                .values()
                .all(|entry| entry.content_md5.is_none())
        );
        assert!(
            remote
                .entries
                .values()
                .all(|entry| entry.content_md5.is_none())
        );
    }
}
