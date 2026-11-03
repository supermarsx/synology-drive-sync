//! The verified-pair index rules, against the in-memory backend (perf-design §2.4-2.6, §2.9).
//!
//! Each adversarial case builds the evidence a previous run would have stored, then a fresh walk
//! and listing in which the facts moved the way the attack moves them, and asserts the pair is
//! not served: it stays in the comparison set, so both sides are read live and an upload follows
//! if the content differs.

use super::*;
use crate::integrity::ContentMd5;
use crate::local::{ChangeTimes, FileIdentity};
use std::path::PathBuf;
use std::time::Duration;

const NOW: i64 = 1_800_000_000;
const OLD: i64 = NOW - 3_600;
const MD5_A: [u8; 16] = [0xaa; 16];
const MD5_B: [u8; 16] = [0xbb; 16];

fn at(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds as u64)
}

fn binding() -> Binding {
    Binding {
        profile: "photos".to_owned(),
        source: "/volume1/photos".to_owned(),
        source_device: 41,
        remote: "/backup/photos".to_owned(),
        server: "nas.example:443/DS920".to_owned(),
    }
}

#[derive(Clone, Copy)]
struct Facts {
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    inode: u64,
    remote_mtime: i64,
    remote_ctime: Option<i64>,
    remote_crtime: Option<i64>,
}

fn facts() -> Facts {
    Facts {
        size: 10,
        mtime_ns: (OLD - 100) * 1_000_000_000,
        ctime_ns: (OLD - 100) * 1_000_000_000,
        inode: 7,
        remote_mtime: OLD - 100,
        remote_ctime: Some(OLD - 100),
        remote_crtime: Some(OLD - 200),
    }
}

fn local_entry(relative: &str, facts: Facts, md5: Option<[u8; 16]>) -> LocalEntry {
    LocalEntry {
        relative: relative.to_owned(),
        full_path: PathBuf::from("/volume1/photos").join(relative),
        kind: EntryKind::File,
        size: facts.size,
        mtime_ms: facts.mtime_ns / 1_000_000,
        identity: FileIdentity {
            device: 41,
            inode: facts.inode,
        },
        times: ChangeTimes {
            mtime_ns: facts.mtime_ns,
            ctime_ns: facts.ctime_ns,
        },
        content_md5: md5.map(ContentMd5::from_bytes),
    }
}

fn remote_entry(relative: &str, facts: Facts, md5: Option<[u8; 16]>) -> RemoteEntry {
    RemoteEntry {
        relative: relative.to_owned(),
        remote_path: format!("/backup/photos/{relative}"),
        kind: EntryKind::File,
        size: facts.size,
        mtime_seconds: facts.remote_mtime,
        ctime_seconds: facts.remote_ctime,
        crtime_seconds: facts.remote_crtime,
        digest_nas_time: md5.map(|_| OLD),
        mount_point_type: None,
        content_md5: md5.map(ContentMd5::from_bytes),
    }
}

/// A local and a remote inventory with one pair per `(path, facts, digest)`.
fn tree(pairs: &[(&str, Facts, Option<[u8; 16]>)]) -> (LocalInventory, RemoteInventory) {
    let mut local = LocalInventory {
        root: PathBuf::from("/volume1/photos"),
        entries: BTreeMap::new(),
        skipped: Vec::new(),
    };
    let mut remote = RemoteInventory {
        root_exists: true,
        entries: BTreeMap::new(),
    };
    for (relative, facts, md5) in pairs {
        local
            .entries
            .insert((*relative).to_owned(), local_entry(relative, *facts, *md5));
        remote
            .entries
            .insert((*relative).to_owned(), remote_entry(relative, *facts, *md5));
    }
    (local, remote)
}

fn keys(inventory: &RemoteInventory) -> BTreeSet<String> {
    inventory.entries.keys().cloned().collect()
}

fn options(canary: usize) -> IndexOptions {
    IndexOptions {
        max_age_seconds: DEFAULT_MAX_AGE_SECONDS,
        canary,
        refresh: false,
    }
}

/// An index holding what a run at `OLD` proved for `pairs` (both digests live and equal).
fn proven(pairs: &[(&str, Facts)]) -> MemoryPairIndex {
    let (local, remote) = tree(
        &pairs
            .iter()
            .map(|(relative, facts)| (*relative, *facts, Some(MD5_A)))
            .collect::<Vec<_>>(),
    );
    let mut session = IndexSession::open(
        Box::new(MemoryPairIndex::default()),
        binding(),
        options(0),
        at(OLD),
    );
    session.harvest(&local, &remote, &keys(&remote), &BTreeMap::new(), at(OLD));
    let index = MemoryPairIndex {
        pairs: std::mem::take(&mut session.upserts).into_iter().collect(),
        meta: Some(IndexMeta {
            schema: SCHEMA.to_owned(),
            semantics_epoch: SEMANTICS_EPOCH,
            binding: binding(),
            remote_signal: RemoteSignal::Ctime,
            canary_at: 0,
        }),
        ..MemoryPairIndex::default()
    };
    assert!(
        index.pairs.values().all(|pair| !pair.record.racy),
        "fixture evidence is old enough to be served"
    );
    index
}

/// What a session on `index` vouches for against a fresh walk and listing holding `fresh`.
fn served(index: MemoryPairIndex, fresh: &[(&str, Facts)]) -> (Vec<String>, IndexSession) {
    let (local, remote) = tree(
        &fresh
            .iter()
            .map(|(relative, facts)| (*relative, *facts, None))
            .collect::<Vec<_>>(),
    );
    let mut session = IndexSession::open(Box::new(index), binding(), options(0), at(NOW));
    let vouched = session.vouch(
        &local,
        &remote,
        &keys(&remote),
        &BTreeSet::new(),
        &BTreeMap::new(),
    );
    (
        vouched.served.into_iter().map(|pair| pair.local).collect(),
        session,
    )
}

#[test]
fn unchanged_facts_are_served_md5_only() {
    let (served, session) = served(proven(&[("a.jpg", facts())]), &[("a.jpg", facts())]);
    assert_eq!(served, vec!["a.jpg"]);
    assert_eq!(session.report.oldest_evidence_epoch, Some(OLD));
    // The type barrier: what the index supplies can never authorise a mutation.
    let digest = ContentMd5::from_bytes(MD5_A);
    assert!(!digest.has_full_proof());
}

/// L1: one byte rewritten in place, mtime restored (`touch -r`). Size, mtime and inode hold; the
/// change time cannot be set back, so the record no longer matches.
#[test]
fn an_in_place_edit_with_its_mtime_restored_is_caught_by_ctime() {
    let mut edited = facts();
    edited.ctime_ns += 5_000_000_000;
    let (served, session) = served(proven(&[("a.jpg", facts())]), &[("a.jpg", edited)]);
    assert!(served.is_empty());
    assert!(session.invalidated.contains("a.jpg"));
}

/// L2/L3: an atomic replace, or a delete and re-create that reuses the inode, keeping size and
/// mtime. Creation and rename stamp a new change time.
#[test]
fn inode_reuse_and_atomic_replace_are_not_served() {
    let mut reused = facts();
    reused.ctime_ns += 1;
    let mut replaced = facts();
    replaced.inode = 8;
    let (served, _) = served(
        proven(&[("reused.jpg", facts()), ("replaced.jpg", facts())]),
        &[("reused.jpg", reused), ("replaced.jpg", replaced)],
    );
    assert!(served.is_empty(), "{served:?}");
}

/// R1: another writer replaces the NAS copy with the same size and mtime (a Drive client,
/// `rsync -t`). Its change time moves; a restore that also resets creation time moves crtime.
#[test]
fn a_remote_replacement_with_the_same_size_and_mtime_is_not_served() {
    let mut rewritten = facts();
    rewritten.remote_ctime = rewritten.remote_ctime.map(|ctime| ctime + 60);
    let mut recreated = facts();
    recreated.remote_crtime = Some(OLD - 10);
    let (served, _) = served(
        proven(&[("rewritten.jpg", facts()), ("recreated.jpg", facts())]),
        &[("rewritten.jpg", rewritten), ("recreated.jpg", recreated)],
    );
    assert!(served.is_empty(), "{served:?}");
}

/// L4/R2: a pair observed within two seconds of a write, on either side, is stored racy and
/// never served, so a write in the same clock tick as the read cannot hide.
#[test]
fn a_write_within_the_racy_window_is_stored_but_never_served() {
    let read_started = OLD;
    let mut local_racy = facts();
    local_racy.ctime_ns = (read_started - 1) * 1_000_000_000;
    let mut remote_racy = facts();
    remote_racy.remote_ctime = Some(OLD - 1); // within 2 s of the MD5.start clock (OLD)
    let (local, remote) = tree(&[
        ("local.jpg", local_racy, Some(MD5_A)),
        ("remote.jpg", remote_racy, Some(MD5_A)),
        ("calm.jpg", facts(), Some(MD5_A)),
    ]);
    let mut writer = IndexSession::open(
        Box::new(MemoryPairIndex::default()),
        binding(),
        options(0),
        at(read_started),
    );
    writer.harvest(
        &local,
        &remote,
        &keys(&remote),
        &BTreeMap::new(),
        at(read_started),
    );
    let racy: Vec<&str> = writer
        .upserts
        .iter()
        .filter(|(_, pair)| pair.record.racy)
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(racy, vec!["local.jpg", "remote.jpg"]);

    let index = MemoryPairIndex {
        pairs: std::mem::take(&mut writer.upserts).into_iter().collect(),
        meta: proven(&[]).meta,
        ..MemoryPairIndex::default()
    };
    let (served, session) = served(
        index,
        &[
            ("local.jpg", local_racy),
            ("remote.jpg", remote_racy),
            ("calm.jpg", facts()),
        ],
    );
    assert_eq!(served, vec!["calm.jpg"]);
    assert_eq!(session.report.racy, 2);
}

/// No NAS clock from `MD5.start` means the remote side's racy window cannot be ruled out.
#[test]
fn evidence_without_a_nas_clock_is_racy() {
    let (local, mut remote) = tree(&[("a.jpg", facts(), Some(MD5_A))]);
    remote.entries.get_mut("a.jpg").unwrap().digest_nas_time = None;
    let mut writer = IndexSession::open(
        Box::new(MemoryPairIndex::default()),
        binding(),
        options(0),
        at(OLD),
    );
    writer.harvest(&local, &remote, &keys(&remote), &BTreeMap::new(), at(OLD));
    assert!(writer.upserts["a.jpg"].record.racy);
}

/// A pair whose two live digests disagree is an upload, never evidence.
#[test]
fn a_differing_pair_is_never_stored() {
    let (mut local, remote) = tree(&[("a.jpg", facts(), Some(MD5_A))]);
    local.entries.get_mut("a.jpg").unwrap().content_md5 = Some(ContentMd5::from_bytes(MD5_B));
    let mut writer = IndexSession::open(
        Box::new(MemoryPairIndex::default()),
        binding(),
        options(0),
        at(OLD),
    );
    writer.harvest(&local, &remote, &keys(&remote), &BTreeMap::new(), at(OLD));
    assert!(writer.upserts.is_empty());
}

/// P1/R4: an index bound to another profile, source, device, remote or server is retired: not
/// served, cleared in this run's commit, and named.
#[test]
fn an_index_from_another_profile_source_or_server_is_retired() {
    for (field, change) in [
        (
            "profile",
            Box::new(|b: &mut Binding| b.profile = "other".into()) as Box<dyn Fn(&mut Binding)>,
        ),
        (
            "source",
            Box::new(|b: &mut Binding| b.source = "/volume2/x".into()),
        ),
        (
            "source device",
            Box::new(|b: &mut Binding| b.source_device = 42),
        ),
        (
            "remote",
            Box::new(|b: &mut Binding| b.remote = "/elsewhere".into()),
        ),
        (
            "server",
            Box::new(|b: &mut Binding| b.server = "other-nas:443/DS".into()),
        ),
    ] {
        let mut index = proven(&[("a.jpg", facts())]);
        let meta = index.meta.as_mut().unwrap();
        change(&mut meta.binding);
        let (served, session) = served(index, &[("a.jpg", facts())]);
        assert!(served.is_empty(), "{field}");
        assert_eq!(session.report.state, IndexState::Cold, "{field}");
        let issue = &session.report.issues[0];
        assert_eq!(issue.code, "pair-index-retired", "{field}");
        assert!(issue.cause.contains(field), "{field}: {}", issue.cause);
        assert!(session.clear, "{field}: cleared in the run's commit");
    }
}

#[test]
fn an_index_from_another_semantics_epoch_is_retired() {
    let mut index = proven(&[("a.jpg", facts())]);
    index.meta.as_mut().unwrap().semantics_epoch = SEMANTICS_EPOCH + 1;
    let (served, session) = served(index, &[("a.jpg", facts())]);
    assert!(served.is_empty());
    assert_eq!(session.report.issues[0].code, "pair-index-retired");
}

/// Expiry rolls across the second half of `max_age` and never serves a record from the future.
#[test]
fn expiry_is_rolling_and_bounded_and_future_evidence_is_expired() {
    let session = IndexSession::open(
        Box::new(MemoryPairIndex::default()),
        binding(),
        options(0),
        at(NOW),
    );
    let record = |verified_at| PairRecord {
        verified_at,
        ..proven(&[("a.jpg", facts())]).pairs["a.jpg"].record
    };
    let half = DEFAULT_MAX_AGE_SECONDS / 2;
    for index in 0..200 {
        let path = format!("folder/{index}.jpg");
        assert!(!session.expired(&path, &record(NOW - half + 1)), "{path}");
        assert!(
            session.expired(&path, &record(NOW - DEFAULT_MAX_AGE_SECONDS - 1)),
            "{path}"
        );
        assert!(session.expired(&path, &record(NOW + 1)), "future: {path}");
    }
    let spread: BTreeSet<bool> = (0..200)
        .map(|index| session.expired(&format!("f{index}"), &record(NOW - half * 3 / 2)))
        .collect();
    assert_eq!(
        spread.len(),
        2,
        "rolling: some expire at 3/4 of max_age, some do not"
    );

    let mut stale = facts();
    stale.size = 10;
    let mut index = proven(&[("a.jpg", stale)]);
    index.pairs.get_mut("a.jpg").unwrap().record.verified_at = NOW - 2 * DEFAULT_MAX_AGE_SECONDS;
    let (served, session) = served(index, &[("a.jpg", stale)]);
    assert!(served.is_empty());
    assert_eq!(session.report.expired, 1);
}

/// P2: a record proven under one remote spelling is served only for exactly that pairing.
#[test]
fn a_case_only_re_pairing_is_not_served() {
    let index = proven(&[("Docs/a.jpg", facts())]);
    let (local, mut remote) = tree(&[("Docs/a.jpg", facts(), None)]);
    let entry = remote.entries.remove("Docs/a.jpg").unwrap();
    remote.entries.insert(
        "docs/a.jpg".to_owned(),
        RemoteEntry {
            relative: "docs/a.jpg".to_owned(),
            remote_path: "/backup/photos/docs/a.jpg".to_owned(),
            ..entry
        },
    );
    let spellings = BTreeMap::from([("docs/a.jpg".to_owned(), "Docs/a.jpg".to_owned())]);
    let mut session = IndexSession::open(Box::new(index), binding(), options(0), at(NOW));
    let vouched = session.vouch(
        &local,
        &remote,
        &keys(&remote),
        &BTreeSet::new(),
        &spellings,
    );
    assert!(vouched.served.is_empty());
}

/// A record for a path with no local counterpart (a deletion candidate) is never consulted, and
/// a pair in the strong set (a deletion guard or server-copy source) is never vouched: those
/// decisions need a full fingerprint read from the bytes.
#[test]
fn deletion_and_server_copy_guards_never_take_a_stored_digest() {
    let index = proven(&[("gone.jpg", facts()), ("source.jpg", facts())]);
    let (mut local, remote) = tree(&[("gone.jpg", facts(), None), ("source.jpg", facts(), None)]);
    local.entries.remove("gone.jpg");
    let strong = BTreeSet::from(["source.jpg".to_owned()]);
    let mut session = IndexSession::open(Box::new(index), binding(), options(0), at(NOW));
    let vouched = session.vouch(&local, &remote, &keys(&remote), &strong, &BTreeMap::new());
    assert!(vouched.served.is_empty() && vouched.canaries.is_empty());
}

/// A canary that disagrees discards the whole index: this run's commit clears it before writing
/// what the run proved live, and the disagreement is a named warning, not a failure.
#[test]
fn a_canary_disagreement_discards_the_whole_index_and_is_named() {
    let index = proven(&[("a.jpg", facts()), ("b.jpg", facts())]);
    let (local, remote) = tree(&[("a.jpg", facts(), None), ("b.jpg", facts(), None)]);
    let mut session = IndexSession::open(Box::new(index), binding(), options(1), at(NOW));
    let vouched = session.vouch(
        &local,
        &remote,
        &keys(&remote),
        &BTreeSet::new(),
        &BTreeMap::new(),
    );
    assert_eq!(vouched.canaries.len(), 1);
    let report = EvidenceReport {
        served: 0,
        verified_live: 2,
        canary_checked: 1,
        canary_inconclusive: 0,
        canary_mismatch: Some(crate::digest_evidence::CanaryMismatch {
            path: vouched.canaries[0].local.clone(),
            vouched: "aaaaaaaaaaaa".to_owned(),
            local: "bbbbbbbbbbbb".to_owned(),
            remote: Some("bbbbbbbbbbbb".to_owned()),
        }),
        fell_back: true,
    };
    // The fallback re-read every pair live; both proved equal under the new digest.
    let (fresh_local, fresh_remote) = tree(&[
        ("a.jpg", facts(), Some(MD5_B)),
        ("b.jpg", facts(), Some(MD5_B)),
    ]);
    session.observe_live(
        &fresh_local,
        &fresh_remote,
        &keys(&fresh_remote),
        &BTreeMap::new(),
        at(NOW),
    );
    session.conclude(&report);
    assert!(session.clear);
    let report = session.commit(Some(&fresh_local), None, true);
    assert_eq!(report.canary_mismatches, 1);
    let issue = report
        .issues
        .iter()
        .find(|issue| issue.code == "pair-index-canary-mismatch")
        .expect("named");
    assert_eq!(issue.severity, Severity::Warning);
    assert_eq!(report.stored, 2);
}

/// The committed index after a discard holds only what this run proved live.
#[test]
fn a_discarded_index_is_rebuilt_from_this_runs_live_evidence() {
    let mut index = proven(&[("a.jpg", facts()), ("old.jpg", facts())]);
    index.pairs.get_mut("a.jpg").unwrap().record.md5 = MD5_B;
    let shared = std::rc::Rc::new(std::cell::RefCell::new(index));
    let mut session = IndexSession::open(
        Box::new(SharedIndex(shared.clone())),
        binding(),
        options(0),
        at(NOW),
    );
    let (local, remote) = tree(&[("a.jpg", facts(), Some(MD5_A))]);
    session.discard();
    session.harvest(&local, &remote, &keys(&remote), &BTreeMap::new(), at(NOW));
    session.commit(Some(&local), None, true);
    let index = shared.borrow();
    assert_eq!(index.pairs.keys().collect::<Vec<_>>(), vec!["a.jpg"]);
    assert_eq!(index.pairs["a.jpg"].record.md5, MD5_A);
}

/// A run that does not finish its digest pass (a breaker trip, a failure) writes nothing.
#[test]
fn an_abandoned_run_leaves_the_index_exactly_as_it_was() {
    let index = proven(&[("a.jpg", facts())]);
    let before = index.pairs.clone();
    let shared = std::rc::Rc::new(std::cell::RefCell::new(index));
    let mut session = IndexSession::open(
        Box::new(SharedIndex(shared.clone())),
        binding(),
        options(0),
        at(NOW),
    );
    let (local, remote) = tree(&[("a.jpg", facts(), Some(MD5_B))]);
    session.harvest(&local, &remote, &keys(&remote), &BTreeMap::new(), at(NOW));
    session.abandon();
    assert_eq!(shared.borrow().pairs, before);
    assert_eq!(shared.borrow().commits, 0);
}

/// Touched paths are removed, served pairs keep their original verification time, and an
/// unscoped run drops records for files that are gone.
#[test]
fn the_commit_removes_touched_and_stale_pairs_and_keeps_served_evidence_untouched() {
    let index = proven(&[
        ("kept.jpg", facts()),
        ("uploaded.jpg", facts()),
        ("deleted.jpg", facts()),
    ]);
    let shared = std::rc::Rc::new(std::cell::RefCell::new(index));
    let mut session = IndexSession::open(
        Box::new(SharedIndex(shared.clone())),
        binding(),
        options(0),
        at(NOW),
    );
    let (local, remote) = tree(&[("kept.jpg", facts(), None), ("uploaded.jpg", facts(), None)]);
    let vouched = session.vouch(
        &local,
        &remote,
        &keys(&remote),
        &BTreeSet::new(),
        &BTreeMap::new(),
    );
    assert_eq!(vouched.served.len(), 2);
    let mut touched = TouchedPaths::default();
    touched.insert("uploaded.jpg");
    let report = session.commit(Some(&local), Some(&touched), true);
    assert_eq!(report.removed, 2);
    let index = shared.borrow();
    assert_eq!(index.pairs.keys().collect::<Vec<_>>(), vec!["kept.jpg"]);
    assert_eq!(index.pairs["kept.jpg"].record.verified_at, OLD);
}

#[test]
fn a_scoped_run_never_sweeps_records_outside_what_it_walked() {
    let index = proven(&[("in/a.jpg", facts()), ("out/b.jpg", facts())]);
    let shared = std::rc::Rc::new(std::cell::RefCell::new(index));
    let session = IndexSession::open(
        Box::new(SharedIndex(shared.clone())),
        binding(),
        options(0),
        at(NOW),
    );
    let (local, _) = tree(&[("in/a.jpg", facts(), None)]);
    session.commit(Some(&local), None, false);
    assert_eq!(shared.borrow().pairs.len(), 2);
}

/// A store that fails (a full disk) is a named warning, and the previous evidence stays.
#[test]
fn a_failed_store_is_named_and_keeps_the_previous_evidence() {
    let mut index = proven(&[("a.jpg", facts())]);
    index.fail_next_commit = Some(IndexIssue::warning(
        "pair-index-store-failed",
        "/cache/pairs/photos.redb",
        "No space left on device",
    ));
    let before = index.pairs.clone();
    let shared = std::rc::Rc::new(std::cell::RefCell::new(index));
    let mut session = IndexSession::open(
        Box::new(SharedIndex(shared.clone())),
        binding(),
        options(0),
        at(NOW),
    );
    let (local, remote) = tree(&[("b.jpg", facts(), Some(MD5_A))]);
    session.harvest(&local, &remote, &keys(&remote), &BTreeMap::new(), at(NOW));
    let report = session.commit(Some(&local), None, true);
    assert_eq!(report.issues[0].code, "pair-index-store-failed");
    assert_eq!(report.stored, 0);
    assert_eq!(shared.borrow().pairs, before);
}

/// A backend that cannot even be read is unusable: nothing is served, nothing is written.
#[test]
fn an_unreadable_index_serves_nothing_and_writes_nothing() {
    struct Broken;
    impl PairIndex for Broken {
        fn meta(&mut self) -> Result<Option<IndexMeta>, IndexIssue> {
            Err(IndexIssue::warning(
                "pair-index-corrupt",
                "/cache/pairs/photos.redb",
                "integrity check failed",
            ))
        }
        fn get(&mut self, _: &str) -> Result<Option<StoredPair>, IndexIssue> {
            unreachable!("a broken index is never read")
        }
        fn for_each_key(&mut self, _: &mut dyn FnMut(&str)) -> Result<(), IndexIssue> {
            unreachable!("a broken index is never swept")
        }
        fn commit(&mut self, _: IndexCommit) -> Result<(), IndexIssue> {
            unreachable!("a broken index is never written")
        }
    }
    let mut session = IndexSession::open(Box::new(Broken), binding(), options(0), at(NOW));
    let (local, remote) = tree(&[("a.jpg", facts(), Some(MD5_A))]);
    let vouched = session.vouch(
        &local,
        &remote,
        &keys(&remote),
        &BTreeSet::new(),
        &BTreeMap::new(),
    );
    assert!(vouched.served.is_empty());
    session.harvest(&local, &remote, &keys(&remote), &BTreeMap::new(), at(NOW));
    let report = session.commit(Some(&local), None, true);
    assert_eq!(report.state, IndexState::Unusable);
    assert_eq!(report.issues[0].code, "pair-index-corrupt");
}

/// `--pair-index-refresh` serves nothing and rewrites everything.
#[test]
fn a_refresh_serves_nothing_and_clears_on_commit() {
    let index = proven(&[("a.jpg", facts())]);
    let mut session = IndexSession::open(
        Box::new(index),
        binding(),
        IndexOptions {
            refresh: true,
            ..options(0)
        },
        at(NOW),
    );
    let (local, remote) = tree(&[("a.jpg", facts(), None)]);
    let vouched = session.vouch(
        &local,
        &remote,
        &keys(&remote),
        &BTreeSet::new(),
        &BTreeMap::new(),
    );
    assert!(vouched.served.is_empty());
    assert_eq!(session.report.state, IndexState::Refreshed);
    assert!(session.clear);
}

/// R5: a run whose link guard did not run everywhere serves nothing.
#[test]
fn an_unchecked_link_guard_suspends_serving() {
    let index = proven(&[("a.jpg", facts())]);
    let mut session = IndexSession::open(Box::new(index), binding(), options(0), at(NOW));
    session.suspend_serving(IndexIssue::info(
        "pair-index-link-guard-unchecked",
        "/backup/photos",
        "3 folders listed without a real path",
    ));
    let (local, remote) = tree(&[("a.jpg", facts(), None)]);
    assert!(
        session
            .vouch(
                &local,
                &remote,
                &keys(&remote),
                &BTreeSet::new(),
                &BTreeMap::new()
            )
            .served
            .is_empty()
    );
}

/// Without a remote change time, only the local read is skipped: File Station's MD5 still runs.
#[test]
fn local_only_mode_vouches_for_the_local_side_alone() {
    let index = proven(&[("a.jpg", facts())]);
    let mut fresh = facts();
    fresh.remote_ctime = None;
    fresh.remote_crtime = None;
    let (local, remote) = tree(&[("a.jpg", fresh, None)]);
    let mut session = IndexSession::open(Box::new(index), binding(), options(0), at(NOW));
    session.observe_listing(&remote);
    let vouched = session.vouch(
        &local,
        &remote,
        &keys(&remote),
        &BTreeSet::new(),
        &BTreeMap::new(),
    );
    assert_eq!(vouched.served.len(), 1);
    assert!(!vouched.served[0].remote_too);
    assert_eq!(
        session.report.issues[0].code,
        "pair-index-remote-ctime-unavailable"
    );
}

/// The self-test: a NAS whose listed change time does not move for this run's own upload has no
/// usable remote change signal.
#[test]
fn a_remote_ctime_that_does_not_show_an_upload_switches_to_local_only() {
    let (_, before) = tree(&[("a.jpg", facts(), None)]);
    let (_, after) = tree(&[("a.jpg", facts(), None)]);
    let mut session = IndexSession::open(
        Box::new(MemoryPairIndex::default()),
        binding(),
        options(0),
        at(NOW),
    );
    session.self_test_uploads(&["a.jpg".to_owned()], &before, &after, Some(NOW - 10));
    assert_eq!(session.signal, RemoteSignal::LocalOnly);
    assert_eq!(
        session.report.issues[0].code,
        "pair-index-remote-ctime-static"
    );

    let mut moved = facts();
    moved.remote_ctime = Some(NOW);
    let (_, after) = tree(&[("a.jpg", moved, None)]);
    let mut session = IndexSession::open(
        Box::new(MemoryPairIndex::default()),
        binding(),
        options(0),
        at(NOW),
    );
    session.self_test_uploads(&["a.jpg".to_owned()], &before, &after, Some(NOW - 10));
    assert_eq!(session.signal, RemoteSignal::Ctime);
}

/// A shared handle on a [`MemoryPairIndex`], so a test can inspect what a session committed.
struct SharedIndex(std::rc::Rc<std::cell::RefCell<MemoryPairIndex>>);

impl PairIndex for SharedIndex {
    fn meta(&mut self) -> Result<Option<IndexMeta>, IndexIssue> {
        self.0.borrow_mut().meta()
    }
    fn get(&mut self, key: &str) -> Result<Option<StoredPair>, IndexIssue> {
        self.0.borrow_mut().get(key)
    }
    fn for_each_key(&mut self, visit: &mut dyn FnMut(&str)) -> Result<(), IndexIssue> {
        self.0.borrow_mut().for_each_key(visit)
    }
    fn commit(&mut self, commit: IndexCommit) -> Result<(), IndexIssue> {
        self.0.borrow_mut().commit(commit)
    }
}
