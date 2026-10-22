use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use crate::api::{RemoteEntry, RemoteInventory};
use crate::cancel::CancellationToken;
use crate::integrity::{ContentMatch, ContentMd5};
use crate::local::{
    EntryKind, IgnoreRules, LocalEntry, LocalInventory, SkipReason, SkippedEntry,
    fold_has_ancestor_in,
};
use crate::path::{RemoteRoot, depth, is_dsm_managed, nfc, portable_fold, validate_relative};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareMode {
    Content,
    Metadata,
    SizeOnly,
    /// Skip the comparison entirely and schedule every in-scope file for upload.
    ///
    /// This is destructive-adjacent: it replaces remote file content that may be identical or
    /// newer. It is never a default, is never implied by [`Scope`], and cannot be set from a
    /// configuration profile. It does not delete on its own; mirror deletion remains behind the
    /// separate delete option.
    Force,
}

impl CompareMode {
    /// Whether this mode requires plan-time content digests for the entries it may delete.
    ///
    /// [`Self::Force`] answers `true` alongside [`Self::Content`]. A deletion guard must never
    /// weaken as a side effect of a flag that was about uploading rather than about guarding:
    /// were `Force` merely "not `Content`", a forced mirror run would silently drop from
    /// digest-guarded deletion to size/mtime-guarded deletion with nothing in the output saying so.
    pub fn requires_deletion_digest(self) -> bool {
        matches!(self, Self::Content | Self::Force)
    }
}

/// A source-relative restriction naming either one directory subtree or one single file.
///
/// The same prefix predicate covers both: a file's relative path simply has no descendants. This
/// is what makes "resync exactly this file" expressible, which pointing the source and destination
/// at a subfolder cannot do.
///
/// Deliberately not a glob. A pattern language here would inherit the fragility that rules out
/// building a UI action on gitignore negation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Scope(String);

impl Scope {
    /// The unrestricted scope: the whole tree.
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Normalize and validate a caller-supplied scope.
    ///
    /// Accepts the same shape as any other relative path in this codebase, so a scope can never
    /// name something the planner would refuse to map. Surrounding slashes are trimmed for
    /// convenience; everything else that `validate_relative` rejects is rejected here.
    pub fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(Self::root());
        }
        if trimmed.contains('\\') {
            return Err(Error::Configuration(format!(
                "scope {value:?} must use forward slashes, not backslashes"
            )));
        }
        validate_relative(trimmed)?;
        if trimmed.split('/').any(|part| part == "." || part == "..") {
            return Err(Error::Configuration(format!(
                "scope {value:?} must not contain . or .. components"
            )));
        }
        if is_dsm_managed(trimmed) {
            return Err(Error::Configuration(format!(
                "scope {value:?} names a DSM-managed directory, which is never synchronized"
            )));
        }
        Ok(Self(trimmed.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `relative` is the scope itself or lies beneath it.
    ///
    /// The comparison is `/`-delimited, so a scope of `docs` matches `docs/a` but never `docsets`.
    pub fn matches(&self, relative: &str) -> bool {
        if self.is_root() {
            return true;
        }
        if relative == self.0 {
            return true;
        }
        relative
            .strip_prefix(&self.0)
            .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Whether `relative` is a strict ancestor directory of the scope.
    ///
    /// Ancestors are walked and inventoried so a scoped plan can still create the parent
    /// directories a scoped upload needs, but they are not themselves in scope and never appear
    /// in a status listing.
    pub fn is_ancestor_of_scope(&self, relative: &str) -> bool {
        if self.is_root() {
            return false;
        }
        if relative.is_empty() {
            return true;
        }
        self.0
            .strip_prefix(relative)
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

#[derive(Clone, Debug)]
pub struct PlanOptions {
    pub delete: bool,
    pub allow_empty_source: bool,
    pub max_delete: usize,
    pub compare: CompareMode,
    pub server_copy: bool,
    /// Restricts which remote entries mirror deletion may consider.
    ///
    /// A scoped inventory also carries the directories between the destination root and the
    /// scope, so that a scoped upload's parents are known to exist and are not re-created. Those
    /// ancestors are outside the scope and have no local counterpart to match, which without this
    /// restriction would present the scope's own parent directories as remote-only and therefore
    /// deletable. Deletion is confined to the scope here rather than left to the empty-source
    /// fuse to catch by accident.
    pub scope: Scope,
}

/// Why the planner scheduled an action. Every variant names what the planner actually observed:
/// either a comparison it performed, or a comparison it could not perform. A mode that never
/// reads content cannot report a content difference, and an uncompared digest is never one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeReason {
    /// No remote entry exists at the mapped path.
    MissingRemote,
    /// Sizes differ. This is the only comparison `--compare size-only` makes.
    SizeDiffers,
    /// Sizes agree and the File Station-resolution modification times differ.
    MtimeDiffers,
    /// Sizes agree and the content digests were both present and did not compare equal.
    ContentDiffers,
    /// Sizes and modification times agree but the local file carries no digest, so no content
    /// comparison happened. The file is scheduled rather than assumed equal.
    LocalDigestUnavailable,
    /// Sizes and modification times agree but no complete remote MD5/CRC32/SHA-256 fingerprint was retrieved, so no content
    /// comparison happened. The file is scheduled rather than assumed equal.
    RemoteDigestUnavailable,
    /// A remote entry of the other kind occupies the path and is replaced.
    TypeReplaced,
    /// No comparison was performed because the run asked for an unconditional re-upload.
    Forced,
}

impl ChangeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingRemote => "missing-remote",
            Self::SizeDiffers => "size-differs",
            Self::MtimeDiffers => "mtime-differs",
            Self::ContentDiffers => "content-differs",
            Self::LocalDigestUnavailable => "local-digest-unavailable",
            Self::RemoteDigestUnavailable => "remote-digest-unavailable",
            Self::TypeReplaced => "type-replaced",
            Self::Forced => "forced",
        }
    }

    /// The comparison behind the reason, phrased for a human plan line.
    pub fn detail(self) -> &'static str {
        match self {
            Self::MissingRemote => "no remote entry at this path",
            Self::SizeDiffers => "local and remote sizes differ",
            Self::MtimeDiffers => "size equal, modification time differs",
            Self::ContentDiffers => {
                "size equal, complete MD5/CRC32/SHA-256 fingerprint did not match"
            }
            Self::LocalDigestUnavailable => {
                "size and time equal, no complete local MD5/CRC32/SHA-256 fingerprint, uploading unverified"
            }
            Self::RemoteDigestUnavailable => {
                "size and time equal, no complete remote MD5/CRC32/SHA-256 fingerprint, uploading unverified"
            }
            Self::TypeReplaced => "remote entry has the conflicting kind",
            Self::Forced => "comparison skipped, forced re-upload overwrites the remote copy",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CreateAction {
    pub relative: String,
    pub remote_path: String,
    pub reason: ChangeReason,
}

#[derive(Clone, Debug)]
pub struct UploadAction {
    pub local: LocalEntry,
    pub remote_path: String,
    pub reason: ChangeReason,
    /// What the destination held at plan time, when this upload overwrites a file.
    ///
    /// `None` when nothing was there (or a directory that a pre-delete removes first). The executor
    /// re-checks it immediately before writing, so an upload never overwrites a remote file that
    /// changed after the plan compared it.
    pub expected_remote: Option<RemoteSnapshot>,
}

#[derive(Clone, Debug)]
pub struct CopyAction {
    pub from_relative: String,
    pub from_remote_path: String,
    pub to_relative: String,
    pub to_remote_path: String,
    pub local: LocalEntry,
    pub expected_size: u64,
    pub content_md5: ContentMd5,
    pub source_snapshot: RemoteSnapshot,
}

#[derive(Clone, Debug)]
pub struct DestinationGuard {
    pub remote_path: String,
    pub local: LocalEntry,
    pub expected_size: u64,
    pub expected_mtime_seconds: i64,
    pub content_md5: ContentMd5,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteSnapshot {
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_seconds: i64,
    pub content_md5: Option<ContentMd5>,
    /// Deleting descendants changes a directory's mtime. Such directories are instead
    /// guarded by their kind, size, observed emptiness, and File Station's nonrecursive delete.
    pub require_mtime: bool,
}

#[derive(Clone, Debug)]
pub struct DeleteAction {
    pub relative: String,
    pub remote_path: String,
    pub kind: EntryKind,
    pub type_conflict: bool,
    pub snapshot: RemoteSnapshot,
    /// Present only when this mirror deletion removes the source of a verified server copy.
    pub destination_guard: Option<DestinationGuard>,
}

#[derive(Clone, Debug)]
pub struct SyncPlan {
    /// The local tree this plan was derived from.
    ///
    /// Carried because execution has to re-ask a question planning already asked: a mirror
    /// deletion's precondition is that the path is absent *locally*, and the only thing that can
    /// re-check it at delete time is the root the inventory was walked from. Every other input to
    /// the plan is either remote or already recorded per action.
    pub source_root: PathBuf,
    /// The part of the destination this plan describes.
    ///
    /// Carried for the same reason as `source_root`: execution takes its own fresh inventories to
    /// re-verify every snapshot it is about to act on, and those inventories have to cover exactly
    /// what the plan covers. Taken at the root instead, a `--scope docs/q3` run paid a full
    /// unscoped recursive walk of a 100,000-entry destination per snapshot batch — up to four per
    /// run, and the very expense `--scope` exists to avoid — and could be failed *after its uploads
    /// had landed* by a subtree it was explicitly told to ignore returning a permission error.
    pub scope: Scope,
    /// The exclusion rules this plan was built under.
    ///
    /// Carried so the executor's inventories prune what the planner's pruned. A walk that descends
    /// into `@eaDir` when the planning walk did not is not merely slower: it answers a different
    /// question about which entries exist.
    pub rules: IgnoreRules,
    pub pre_deletes: Vec<DeleteAction>,
    pub creates: Vec<CreateAction>,
    pub copies: Vec<CopyAction>,
    pub uploads: Vec<UploadAction>,
    pub post_deletes: Vec<DeleteAction>,
    pub unchanged_files: usize,
    pub protected_entries: usize,
    pub upload_bytes: u64,
    /// Paths this plan neither carries nor deletes, and why, sorted by path.
    ///
    /// The local walk's skips, plus the spelling conflicts the planner could not resolve. Each one
    /// used to fail the whole run; now only its own path is left alone, on both sides, and the run
    /// reports it.
    pub skipped: Vec<SkippedEntry>,
}

impl SyncPlan {
    pub fn delete_count(&self) -> usize {
        self.pre_deletes.len() + self.post_deletes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pre_deletes.is_empty()
            && self.creates.is_empty()
            && self.copies.is_empty()
            && self.uploads.is_empty()
            && self.post_deletes.is_empty()
    }
}

/// Select the minimum remote files whose content is needed to build a content-mode plan.
/// Same-path equal-size files need a digest for comparison. Remote-only files are considered
/// only when CopyMove is available and their size/basename can satisfy a supported copy action.
pub fn select_remote_content_hashes(
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    server_copy: bool,
) -> BTreeSet<String> {
    select_remote_content_hashes_for_plan(local, remote, rules, server_copy, false)
}

/// Select the remote files whose digests guard a mirror deletion, and nothing else.
///
/// This is the subset [`select_remote_content_hashes_for_plan`] adds for `delete = true`. It is
/// exposed separately for [`CompareMode::Force`], which performs no comparison and therefore needs
/// no comparison digests, but must still guard every entry it could delete exactly as content mode
/// does. The cost is bounded by the delete-candidate set rather than by the whole scope.
pub fn select_deletion_guard_hashes(
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
) -> BTreeSet<String> {
    let pairs = Correspondence::build(local, remote);
    let mut selected = BTreeSet::new();
    for entry in remote.entries.values() {
        // An empty file's content is known without reading it, so its deletion is guarded by its
        // size alone. See `deletion_snapshot`.
        if entry.kind != EntryKind::File || entry.size == 0 || is_protected(entry, rules) {
            continue;
        }
        let is_delete_candidate = pairs
            .local_of(local, &entry.relative)
            .is_none_or(|local_entry| local_entry.kind != EntryKind::File);
        if is_delete_candidate {
            selected.insert(entry.relative.clone());
        }
    }
    selected
}

/// Select the remote files a same-path comparison needs a digest for, and nothing else.
///
/// These are the entries where local and remote agree on size *and* modification time, so only a
/// digest can separate "identical" from "changed without moving either". Every other pairing is
/// already decided: a size difference short-circuits before any digest is read, and a modification
/// time difference is reported as such.
///
/// This set is the whole tree in the steady state, which is exactly why its digests must be cheap
/// to obtain. Nothing here guards a mutation -- the answer only decides whether to upload -- so an
/// MD5 comparison is sufficient and [`select_strong_remote_digests`] carries the rest.
pub fn select_comparison_remote_digests(
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
) -> BTreeSet<String> {
    let pairs = Correspondence::build(local, remote);
    let mut selected = BTreeSet::new();
    for local_entry in local.entries.values() {
        // Two empty files are identical by definition, so `compare_files` never asks for their
        // digests. Asking DSM to hash one anyway costs a background task per empty file, and its
        // answer for an empty file is exactly the behaviour nobody has verified.
        if local_entry.kind != EntryKind::File || local_entry.size == 0 {
            continue;
        }
        let Some(remote_entry) = pairs.remote_of(remote, &local_entry.relative) else {
            continue;
        };
        if remote_entry.kind == EntryKind::File
            && remote_entry.size == local_entry.size
            && mtimes_agree(local_entry.mtime_ms, remote_entry.mtime_seconds)
            && !is_protected(remote_entry, rules)
        {
            selected.insert(remote_entry.relative.clone());
        }
    }
    selected
}

/// Select the remote files whose digest guards a mutation and must therefore be strong.
///
/// Deletion guards and server-copy sources are grouped because they share the property that
/// separates them from comparison: the digest does not decide whether to upload, it authorises
/// destroying or duplicating remote data. Acting on an MD5 agreement alone would be a weaker claim
/// than this code makes today, so these keep requiring a full MD5/CRC32/SHA-256 fingerprint.
///
/// Obtaining one means downloading the file, which is affordable here and not for comparison
/// because of what the two sets contain. Both halves of this one are remote entries with **no
/// local file counterpart**, which makes it disjoint from the comparison set and near-empty in the
/// steady state this cost matters in: nothing is deleted and nothing is server-copied when nothing
/// has changed. Its cost is therefore proportional to how much is actually being deleted or
/// copied, not to how much exists -- with the honest exception of a first mirror run against an
/// already-populated destination, where everything really is a deletion candidate and downloading
/// it is the price of deleting it on strong evidence.
pub fn select_strong_remote_digests(
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    server_copy: bool,
    delete: bool,
) -> BTreeSet<String> {
    let mut selected = BTreeSet::new();
    if delete {
        selected.extend(select_deletion_guard_hashes(local, remote, rules));
    }
    if !server_copy {
        return selected;
    }

    let pairs = Correspondence::build(local, remote);
    let mut missing_by_size_name_and_mtime: BTreeMap<(u64, String, i64), BTreeSet<String>> =
        BTreeMap::new();
    for entry in local.entries.values() {
        if entry.kind != EntryKind::File
            || entry.size == 0
            || pairs.remote_of(remote, &entry.relative).is_some()
        {
            continue;
        }
        let (parent, name) = relative_parent_and_name(&entry.relative);
        missing_by_size_name_and_mtime
            .entry((entry.size, name.to_owned(), local_mtime_seconds(entry)))
            .or_default()
            .insert(parent.to_owned());
    }

    for entry in remote.entries.values() {
        if entry.kind != EntryKind::File
            || entry.size == 0
            || pairs.local_of(local, &entry.relative).is_some()
            || is_protected(entry, rules)
            || has_local_file_ancestor(&entry.relative, local)
        {
            continue;
        }
        let (parent, name) = relative_parent_and_name(&entry.relative);
        let Some(target_parents) =
            missing_by_size_name_and_mtime.get(&(entry.size, name.to_owned(), entry.mtime_seconds))
        else {
            continue;
        };
        if target_parents.iter().any(|target| target != parent) {
            selected.insert(entry.relative.clone());
        }
    }
    selected
}

/// Select remote content needed for comparison, optional server-copy reuse, and deletion guards.
/// A content-mode mirror must pass `delete = true` so every file that could be removed has a
/// plan-time digest. `build_plan` fails closed if such a digest is absent.
///
/// The union of the two selections above, kept for callers that want one set regardless of how
/// each digest will be obtained.
pub fn select_remote_content_hashes_for_plan(
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    server_copy: bool,
    delete: bool,
) -> BTreeSet<String> {
    let mut selected = select_comparison_remote_digests(local, remote, rules);
    selected.extend(select_strong_remote_digests(
        local,
        remote,
        rules,
        server_copy,
        delete,
    ));
    selected
}

/// Raise every planned upload's fingerprint to full strength before anything is written.
///
/// The other half of [`crate::local::populate_content_md5_selective`]. Comparison-set files are
/// hashed MD5-only because their remote counterpart carries nothing stronger to compare against;
/// a file that turns out to *differ* leaves that set and becomes an upload, and uploads verify
/// against a strong digest — `preflight_upload_source` re-hashes before writing and `upload_file`
/// re-hashes after, both comparing with `full_match`, which cannot succeed unless both sides carry
/// SHA-256.
///
/// Must therefore run after the plan is built and before it is executed. Skipping it would not
/// fail loudly at the point of the mistake: those checks are `if let Some(expected)` guarded, so a
/// weak digest turns a verified upload into a rejected one rather than an unverified one — noisy
/// rather than silent, but wrong either way, and only because `full_match` refuses to answer on
/// partial evidence. The strength is asserted here rather than assumed downstream.
///
/// Costs one extra read of each file that actually changed, which is about to be read again to be
/// uploaded. In the steady state that motivated the narrowing, nothing has changed and this does
/// nothing at all.
pub fn promote_upload_fingerprints(
    plan: &mut SyncPlan,
    cancellation: &CancellationToken,
) -> Result<()> {
    for upload in &mut plan.uploads {
        cancellation.check()?;
        crate::local::promote_to_full_fingerprint(&mut upload.local, cancellation)?;
    }
    Ok(())
}

pub fn build_plan(
    root: &RemoteRoot,
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    options: &PlanOptions,
) -> Result<SyncPlan> {
    // The selected destination prefix is part of every Synology Drive-visible path. Validate the
    // complete mapping even for entries that may ultimately compare as unchanged, then reject
    // case variants across either inventory before File Station can create a second spelling.
    for relative in local.entries.keys() {
        root.join(relative)?;
    }
    let pairs = Correspondence::build(local, remote);
    let (shield, mut skipped) = resolve_spellings(local, remote, rules, options.delete);
    skipped.extend(local.skipped.iter().cloned());
    skipped.sort_by(|left, right| left.relative.cmp(&right.relative));

    let mut plan = SyncPlan {
        source_root: local.root.clone(),
        scope: options.scope.clone(),
        rules: rules.clone(),
        pre_deletes: Vec::new(),
        creates: Vec::new(),
        copies: Vec::new(),
        uploads: Vec::new(),
        post_deletes: Vec::new(),
        unchanged_files: 0,
        protected_entries: 0,
        upload_bytes: 0,
        skipped,
    };

    if !remote.root_exists {
        if root.as_str() == root.share_path() {
            return Err(Error::ShareNotWritable(root.share_name().to_owned()));
        }
        plan.creates.push(CreateAction {
            relative: String::new(),
            remote_path: root.as_str().to_owned(),
            reason: ChangeReason::MissingRemote,
        });
    }

    // File Station can expose CIFS/NFS/ISO mounts as directories. Their contents belong
    // to another filesystem, so never synthesize uploads beneath a pruned mount boundary.
    for mount in remote
        .entries
        .values()
        .filter(|entry| entry.mount_point_type.is_some())
    {
        let prefix = format!("{}/", mount.relative);
        if local
            .entries
            .keys()
            .any(|relative| pairs.remote_relative(relative).starts_with(&prefix))
        {
            return Err(Error::ProtectedConflict(mount.remote_path.clone()));
        }
    }

    let mut predeleted = BTreeSet::new();
    for entry in local.entries.values() {
        if shield.covers(&entry.relative) {
            continue;
        }
        // The remote spelling of this path: the local one, unless the destination already holds
        // the same name under a different Unicode normalisation. Writing to the local spelling
        // then would create a second, visually identical entry beside it.
        let remote_relative = pairs.remote_relative(&entry.relative);
        let Some(remote_entry) = remote.entries.get(remote_relative.as_ref()) else {
            schedule_missing(root, &mut plan, entry, &remote_relative)?;
            continue;
        };

        match (entry.kind, remote_entry.kind) {
            (EntryKind::Directory, EntryKind::Directory) => {}
            (EntryKind::File, EntryKind::File) => {
                match compare_files(entry, remote_entry, options.compare) {
                    None => plan.unchanged_files += 1,
                    Some(reason) => schedule_upload(
                        root,
                        &mut plan,
                        entry,
                        &remote_relative,
                        reason,
                        Some(overwritten_snapshot(remote_entry)),
                    )?,
                }
            }
            (local_kind, remote_kind) if !options.delete => {
                return Err(Error::TypeConflict {
                    path: remote_entry.remote_path.clone(),
                    local_kind: local_kind.as_str(),
                    remote_kind: remote_kind.as_str(),
                });
            }
            (EntryKind::Directory, EntryKind::File) => {
                push_predelete(
                    &mut plan,
                    remote_entry,
                    &remote.entries,
                    options.compare,
                    &mut predeleted,
                )?;
                schedule_create(
                    root,
                    &mut plan,
                    entry,
                    &remote_relative,
                    ChangeReason::TypeReplaced,
                )?;
            }
            (EntryKind::File, EntryKind::Directory) => {
                let subtree = remote_subtree(&remote.entries, &remote_entry.relative);
                if subtree.iter().any(|candidate| {
                    is_protected(candidate, rules) || shield.covers(&candidate.relative)
                }) {
                    return Err(Error::ProtectedConflict(remote_entry.remote_path.clone()));
                }
                for candidate in subtree {
                    push_predelete(
                        &mut plan,
                        candidate,
                        &remote.entries,
                        options.compare,
                        &mut predeleted,
                    )?;
                }
                // The directory is pre-deleted first, so the path is expected to be empty.
                schedule_upload(
                    root,
                    &mut plan,
                    entry,
                    &remote_relative,
                    ChangeReason::TypeReplaced,
                    None,
                )?;
            }
        }
    }

    if options.compare == CompareMode::Content {
        replace_uploads_with_server_copies(
            &mut plan, local, remote, rules, options, &pairs, &shield,
        )?;
    }

    if options.delete {
        let mut protected_dirs = HashSet::new();
        for entry in remote.entries.values() {
            if is_protected(entry, rules) {
                plan.protected_entries += 1;
                add_ancestor_directories(&entry.relative, &mut protected_dirs);
            } else if shield.covers(&entry.relative) {
                // Kept, so its ancestors stay non-empty and must be kept too: a non-recursive
                // delete of one would only be refused by DSM, failing the run.
                add_ancestor_directories(&entry.relative, &mut protected_dirs);
            }
        }

        for entry in remote.entries.values() {
            if pairs.local_of(local, &entry.relative).is_some()
                || predeleted.contains(&entry.relative)
            {
                continue;
            }
            // Never propose deleting something the run was told not to look at.
            if !options.scope.matches(&entry.relative) {
                continue;
            }
            if is_protected(entry, rules)
                || (entry.kind == EntryKind::Directory && protected_dirs.contains(&entry.relative))
            {
                continue;
            }
            // A path the local walk skipped, or whose spelling the plan could not resolve, is
            // not absent locally. Its state is unknown, and unknown is never grounds to delete.
            if shield.covers(&entry.relative) {
                continue;
            }
            let destination_guard = plan
                .copies
                .iter()
                .find(|copy| copy.from_relative == entry.relative)
                .map(|copy| DestinationGuard {
                    remote_path: copy.to_remote_path.clone(),
                    local: copy.local.clone(),
                    expected_size: copy.expected_size,
                    expected_mtime_seconds: local_mtime_seconds(&copy.local),
                    content_md5: copy.content_md5,
                });
            plan.post_deletes.push(DeleteAction {
                relative: entry.relative.clone(),
                remote_path: entry.remote_path.clone(),
                kind: entry.kind,
                type_conflict: false,
                snapshot: deletion_snapshot(entry, &remote.entries, options.compare)?,
                destination_guard,
            });
        }
    }

    sort_plan(&mut plan);
    let delete_count = plan.delete_count();
    if options.delete && local.files() == 0 && delete_count > 0 && !options.allow_empty_source {
        return Err(Error::EmptySourceDeletion);
    }
    if delete_count > options.max_delete {
        return Err(Error::DeleteLimit {
            planned: delete_count,
            maximum: options.max_delete,
        });
    }
    Ok(plan)
}

/// How local spellings pair with remote ones whose bytes differ but whose name does not.
///
/// Exact byte equality pairs almost everything, and then this holds nothing and costs one
/// `is_ascii` per key. What it adds is the pair whose spellings differ only by Unicode
/// normalisation: macOS writes `café` decomposed (NFD), Windows and Synology Drive write it composed
/// (NFC), and DSM stores whichever bytes it was given. Keyed by bytes alone, the two were planned
/// as an upload of the NFD name -- a second, visually identical remote entry -- and, under
/// `--delete`, a deletion of the NFC one. On APFS the executor's local re-check then found the
/// "deleted" name present, kept it and re-uploaded it, and every later run repeated the cycle and
/// failed reconciliation.
///
/// Each side keeps its own bytes for I/O. Only the pairing is normalisation-insensitive: a local
/// entry is read from its local spelling and written to its partner's remote spelling.
#[derive(Debug, Default)]
pub(crate) struct Correspondence {
    /// Local spelling to remote spelling, for pairs that differ.
    to_remote: BTreeMap<String, String>,
    /// The reverse.
    to_local: BTreeMap<String, String>,
}

impl Correspondence {
    pub(crate) fn build(local: &LocalInventory, remote: &RemoteInventory) -> Self {
        let mut pairs = Self::default();
        let local_odd = non_nfc_by_nfc(local.entries.keys());
        let remote_odd = non_nfc_by_nfc(remote.entries.keys());
        if local_odd.is_empty() && remote_odd.is_empty() {
            return pairs;
        }
        // Only an unambiguous one-to-one pairing is made. Anything else is a collision, which
        // `resolve_spellings` reports and shields, so leaving it unpaired here is never acted on.
        for (composed, spellings) in &local_odd {
            let [local_spelling] = spellings.as_slice() else {
                continue;
            };
            if remote.entries.contains_key(*local_spelling) {
                continue;
            }
            let mut candidates: Vec<&str> = remote_odd.get(composed).cloned().unwrap_or_default();
            if remote.entries.contains_key(composed.as_str()) {
                candidates.push(composed);
            }
            let [remote_spelling] = candidates.as_slice() else {
                continue;
            };
            if local.entries.contains_key(*remote_spelling) {
                continue;
            }
            pairs.insert(local_spelling, remote_spelling);
        }
        for (composed, spellings) in &remote_odd {
            let [remote_spelling] = spellings.as_slice() else {
                continue;
            };
            if pairs.to_local.contains_key(*remote_spelling)
                || local.entries.contains_key(*remote_spelling)
                || local_odd.contains_key(composed)
                || remote.entries.contains_key(composed.as_str())
                || !local.entries.contains_key(composed.as_str())
            {
                continue;
            }
            pairs.insert(composed, remote_spelling);
        }
        pairs
    }

    fn insert(&mut self, local: &str, remote: &str) {
        self.to_remote.insert(local.to_owned(), remote.to_owned());
        self.to_local.insert(remote.to_owned(), local.to_owned());
    }

    /// The remote spelling of a local path.
    ///
    /// A path inside a paired directory takes the directory's remote spelling even when it has no
    /// remote partner of its own yet. Otherwise a new file in the NFD `café/` would be uploaded to
    /// an NFD parent, and DSM would create that second directory to hold it.
    pub(crate) fn remote_relative<'a>(&self, local_relative: &'a str) -> Cow<'a, str> {
        if self.to_remote.is_empty() {
            return Cow::Borrowed(local_relative);
        }
        if let Some(remote) = self.to_remote.get(local_relative) {
            return Cow::Owned(remote.clone());
        }
        let mut current = local_relative;
        while let Some((parent, _)) = current.rsplit_once('/') {
            if let Some(remote) = self.to_remote.get(parent) {
                return Cow::Owned(format!("{remote}{}", &local_relative[parent.len()..]));
            }
            current = parent;
        }
        Cow::Borrowed(local_relative)
    }

    pub(crate) fn remote_of<'r>(
        &self,
        remote: &'r RemoteInventory,
        local_relative: &str,
    ) -> Option<&'r RemoteEntry> {
        remote
            .entries
            .get(self.remote_relative(local_relative).as_ref())
    }

    pub(crate) fn local_of<'l>(
        &self,
        local: &'l LocalInventory,
        remote_relative: &str,
    ) -> Option<&'l LocalEntry> {
        match self.to_local.get(remote_relative) {
            Some(local_relative) => local.entries.get(local_relative),
            None => local.entries.get(remote_relative),
        }
    }

    /// True for a remote spelling that is the partner of a differently spelled local entry.
    pub(crate) fn is_paired_remote(&self, remote_relative: &str) -> bool {
        self.to_local.contains_key(remote_relative)
    }
}

/// The keys that are not already NFC, grouped by their NFC form.
fn non_nfc_by_nfc<'a>(keys: impl Iterator<Item = &'a String>) -> BTreeMap<String, Vec<&'a str>> {
    let mut odd: BTreeMap<String, Vec<&'a str>> = BTreeMap::new();
    for key in keys {
        if let Cow::Owned(composed) = nfc(key) {
            odd.entry(composed).or_default().push(key);
        }
    }
    odd
}

/// Paths a plan must neither carry nor delete: what the local walk skipped, and spelling classes
/// the plan could not resolve.
///
/// Held as portable folds, so a skipped `Docs/` also shields a remote `DOCS/` beneath it. On a
/// case-insensitive source those are one directory, and the walk never looked inside it.
#[derive(Debug, Default)]
pub(crate) struct Shield {
    folds: BTreeSet<String>,
}

impl Shield {
    /// True for `relative` itself and everything beneath a shielded path.
    pub(crate) fn covers(&self, relative: &str) -> bool {
        if self.folds.is_empty() {
            return false;
        }
        let fold = portable_fold(relative);
        self.folds.contains(&fold) || fold_has_ancestor_in(&fold, &self.folds)
    }
}

/// Up to two distinct spellings per side of one portable fold.
#[derive(Debug, Default)]
struct Spellings<'a> {
    local: Option<&'a str>,
    local_other: Option<&'a str>,
    remote: Option<&'a str>,
    remote_other: Option<&'a str>,
}

fn note_spelling<'a>(first: &mut Option<&'a str>, other: &mut Option<&'a str>, spelling: &'a str) {
    match first {
        None => *first = Some(spelling),
        Some(existing) if *existing == spelling => {}
        Some(_) => {
            if other.is_none() {
                *other = Some(spelling);
            }
        }
    }
}

/// Decide, for every path, whether its spelling can be carried, and shield the ones that cannot.
///
/// Synology Drive clients cannot hold two names that differ only by case or Unicode normalisation.
/// This used to be enforced by failing the whole run at the first such pair, which made a case-only
/// rename -- `Report.pdf` to `report.pdf`, `Photos/` to `photos/`, an ordinary action on Windows and
/// macOS -- a permanent failure that not even `--delete` could get past. Each fold class is now
/// decided on its own:
///
/// * one spelling on each side, the same bytes: the ordinary case, nothing to do;
/// * one spelling on each side, equal after NFC: the same name, paired by [`Correspondence`];
/// * one spelling on each side, differing by case: a rename. A mirror run carries it as an upload
///   of the new spelling plus a deletion of the old one, so the run converges. Without `--delete`
///   the old spelling cannot be removed, and writing the new one beside it would create exactly the
///   pair Drive clients cannot hold, so the class is skipped and reported;
/// * two spellings on one side: a genuine collision. Skipped and reported, both spellings untouched.
///
/// A skipped class shields its whole subtree on both sides, and every other path is planned as
/// usual. Protected remote entries take no part, because the plan never touches them: a recycle bin
/// holding `Report.pdf` and `report.pdf` from different days is not a conflict.
fn resolve_spellings(
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    delete: bool,
) -> (Shield, Vec<SkippedEntry>) {
    let mut classes: BTreeMap<String, Spellings<'_>> = BTreeMap::new();
    let considered = local
        .entries
        .keys()
        .map(|relative| (relative.as_str(), true))
        .chain(
            remote
                .entries
                .values()
                .filter(|entry| !is_protected(entry, rules))
                .map(|entry| (entry.relative.as_str(), false)),
        );
    for (relative, is_local) in considered {
        let fold = portable_fold(relative);
        // Every ancestor is decided as well as the path itself. The fold of a prefix is the same
        // prefix of the fold, cut at the same `/`, so one fold per path serves all of them.
        let fold_cuts = fold
            .match_indices('/')
            .map(|(index, _)| index)
            .chain(std::iter::once(fold.len()));
        let spelling_cuts = relative
            .match_indices('/')
            .map(|(index, _)| index)
            .chain(std::iter::once(relative.len()));
        for (fold_cut, spelling_cut) in fold_cuts.zip(spelling_cuts) {
            let key = &fold[..fold_cut];
            if !classes.contains_key(key) {
                classes.insert(key.to_owned(), Spellings::default());
            }
            let class = classes.get_mut(key).expect("inserted above");
            let spelling = &relative[..spelling_cut];
            if is_local {
                note_spelling(&mut class.local, &mut class.local_other, spelling);
            } else {
                note_spelling(&mut class.remote, &mut class.remote_other, spelling);
            }
        }
    }

    // The walk's own skips come first: they are reported already, and anything under them is
    // covered without a second report.
    let mut blocked: BTreeSet<String> = local
        .skipped
        .iter()
        .map(|skip| portable_fold(&skip.relative))
        .collect();
    let mut skipped = Vec::new();
    // Folds sort every ancestor before its descendants, so a blocked class is always seen before
    // anything beneath it, which it already covers and does not report again.
    for (fold, class) in &classes {
        if blocked.contains(fold) || fold_has_ancestor_in(fold, &blocked) {
            continue;
        }
        let mut reports: Vec<(&str, SkipReason)> = Vec::new();
        if let (Some(first), Some(second)) = (class.local, class.local_other) {
            reports.push((
                first,
                SkipReason::LocalCaseCollision {
                    other: second.to_owned(),
                },
            ));
            reports.push((
                second,
                SkipReason::LocalCaseCollision {
                    other: first.to_owned(),
                },
            ));
        } else if let (Some(first), Some(second)) = (class.remote, class.remote_other) {
            reports.push((
                first,
                SkipReason::RemoteCaseCollision {
                    other: second.to_owned(),
                },
            ));
            reports.push((
                second,
                SkipReason::RemoteCaseCollision {
                    other: first.to_owned(),
                },
            ));
            if let Some(local_spelling) = class
                .local
                .filter(|spelling| *spelling != first && *spelling != second)
            {
                reports.push((
                    local_spelling,
                    SkipReason::RemoteCaseCollision {
                        other: first.to_owned(),
                    },
                ));
            }
        } else if let (Some(local_spelling), Some(remote_spelling)) = (class.local, class.remote)
            && local_spelling != remote_spelling
            && nfc(local_spelling) != nfc(remote_spelling)
            && !delete
        {
            reports.push((
                local_spelling,
                SkipReason::CaseRenameNeedsDelete {
                    remote: remote_spelling.to_owned(),
                },
            ));
        }
        if reports.is_empty() {
            continue;
        }
        blocked.insert(fold.clone());
        skipped.extend(reports.into_iter().map(|(relative, reason)| SkippedEntry {
            relative: relative.to_owned(),
            reason,
        }));
    }

    (Shield { folds: blocked }, skipped)
}

fn schedule_missing(
    root: &RemoteRoot,
    plan: &mut SyncPlan,
    entry: &LocalEntry,
    remote_relative: &str,
) -> Result<()> {
    match entry.kind {
        EntryKind::Directory => schedule_create(
            root,
            plan,
            entry,
            remote_relative,
            ChangeReason::MissingRemote,
        ),
        EntryKind::File => schedule_upload(
            root,
            plan,
            entry,
            remote_relative,
            ChangeReason::MissingRemote,
            None,
        ),
    }
}

fn schedule_create(
    root: &RemoteRoot,
    plan: &mut SyncPlan,
    entry: &LocalEntry,
    remote_relative: &str,
    reason: ChangeReason,
) -> Result<()> {
    plan.creates.push(CreateAction {
        relative: entry.relative.clone(),
        remote_path: root.join(remote_relative)?,
        reason,
    });
    Ok(())
}

fn schedule_upload(
    root: &RemoteRoot,
    plan: &mut SyncPlan,
    entry: &LocalEntry,
    remote_relative: &str,
    reason: ChangeReason,
    expected_remote: Option<RemoteSnapshot>,
) -> Result<()> {
    plan.upload_bytes = plan.upload_bytes.saturating_add(entry.size);
    plan.uploads.push(UploadAction {
        local: entry.clone(),
        remote_path: root.join(remote_relative)?,
        reason,
        expected_remote,
    });
    Ok(())
}

/// The plan-time state of a remote file an upload is about to overwrite.
fn overwritten_snapshot(remote: &RemoteEntry) -> RemoteSnapshot {
    RemoteSnapshot {
        kind: remote.kind,
        size: remote.size,
        mtime_seconds: remote.mtime_seconds,
        content_md5: remote.content_md5,
        require_mtime: true,
    }
}

fn push_predelete(
    plan: &mut SyncPlan,
    remote: &RemoteEntry,
    remote_entries: &BTreeMap<String, RemoteEntry>,
    compare: CompareMode,
    predeleted: &mut BTreeSet<String>,
) -> Result<()> {
    if predeleted.insert(remote.relative.clone()) {
        plan.pre_deletes.push(DeleteAction {
            relative: remote.relative.clone(),
            remote_path: remote.remote_path.clone(),
            kind: remote.kind,
            type_conflict: true,
            snapshot: deletion_snapshot(remote, remote_entries, compare)?,
            destination_guard: None,
        });
    }
    Ok(())
}

/// Compare a same-path file pair under `mode`, returning `None` when the mode considers them
/// equal and otherwise the single observation that decided the difference. The reason is limited
/// to what the mode actually inspects: `SizeOnly` can only ever report a size difference, and
/// content mode reports a missing digest as such rather than as a digest mismatch.
fn compare_files(
    local: &LocalEntry,
    remote: &RemoteEntry,
    mode: CompareMode,
) -> Option<ChangeReason> {
    // Checked before the size comparison: a forced run performs no comparison at all, so it must
    // not report a size difference it never consulted.
    if mode == CompareMode::Force {
        return Some(ChangeReason::Forced);
    }
    if local.size != remote.size {
        return Some(ChangeReason::SizeDiffers);
    }
    let mtime_differs = !mtimes_agree(local.mtime_ms, remote.mtime_seconds);
    match mode {
        CompareMode::Force => Some(ChangeReason::Forced),
        CompareMode::SizeOnly => None,
        CompareMode::Metadata => mtime_differs.then_some(ChangeReason::MtimeDiffers),
        // Two empty files are identical by definition. Their digests are never selected, so
        // without this the pair would read as an unverifiable difference and upload every run.
        CompareMode::Content if local.size == 0 => {
            mtime_differs.then_some(ChangeReason::MtimeDiffers)
        }
        CompareMode::Content => match (local.content_md5, remote.content_md5) {
            (Some(local_digest), Some(remote_digest)) => {
                // `Md5Only` is the normal outcome now, not a degraded one: remote
                // comparison digests come from File Station's server-side MD5, which is
                // the only digest it computes without transferring the file. Treating it
                // as equality is the whole point of the change -- and is a decision about
                // *comparison* only. Deletion and server-copy still require strong proof
                // and obtain it separately; see `select_remote_digests`.
                match local_digest.compare_content(&remote_digest) {
                    ContentMatch::Strong | ContentMatch::Md5Only => {
                        mtime_differs.then_some(ChangeReason::MtimeDiffers)
                    }
                    ContentMatch::Differs => Some(ChangeReason::ContentDiffers),
                }
            }
            // A digest is unavailable, so content equality was never established. Name the
            // metadata difference when there is one rather than an unperformed content check.
            _ if mtime_differs => Some(ChangeReason::MtimeDiffers),
            // Nothing distinguishes the pair except an absent digest. Upload anyway, and name
            // the side that could not be hashed instead of claiming the fingerprints disagreed. A local
            // digest is the precondition for the remote lookup, so it is named first.
            (None, _) => Some(ChangeReason::LocalDigestUnavailable),
            (Some(_), None) => Some(ChangeReason::RemoteDigestUnavailable),
        },
    }
}

fn replace_uploads_with_server_copies(
    plan: &mut SyncPlan,
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    options: &PlanOptions,
    pairs: &Correspondence,
    shield: &Shield,
) -> Result<()> {
    if !options.server_copy {
        return Ok(());
    }

    let predeleted: BTreeSet<&str> = plan
        .pre_deletes
        .iter()
        .map(|action| action.relative.as_str())
        .collect();
    let mut local_by_content: BTreeMap<(u64, ContentMd5, i64), Vec<usize>> = BTreeMap::new();
    for (index, upload) in plan.uploads.iter().enumerate() {
        if upload.local.size == 0 || pairs.remote_of(remote, &upload.local.relative).is_some() {
            continue;
        }
        let digest = upload.local.content_md5.ok_or_else(|| {
            Error::Message("content comparison requires every local file digest".to_owned())
        })?;
        if !digest.has_full_proof() {
            return Err(Error::Message(
                "content comparison requires every local MD5/CRC32/SHA-256 fingerprint".to_owned(),
            ));
        }
        local_by_content
            .entry((
                upload.local.size,
                digest,
                local_mtime_seconds(&upload.local),
            ))
            .or_default()
            .push(index);
    }

    let mut remote_by_content: BTreeMap<(u64, ContentMd5, i64), Vec<&RemoteEntry>> =
        BTreeMap::new();
    for entry in remote.entries.values() {
        if entry.kind != EntryKind::File
            || entry.size == 0
            || pairs.local_of(local, &entry.relative).is_some()
            || is_protected(entry, rules)
            || predeleted.contains(entry.relative.as_str())
            || shield.covers(&entry.relative)
        {
            continue;
        }
        let Some(digest) = entry.content_md5 else {
            continue;
        };
        if !digest.has_full_proof() {
            continue;
        }
        remote_by_content
            .entry((entry.size, digest, entry.mtime_seconds))
            .or_default()
            .push(entry);
    }

    let mut replacements = BTreeMap::new();
    for ((expected_size, digest, expected_mtime_seconds), local_candidates) in local_by_content {
        let Some(remote_candidates) =
            remote_by_content.get(&(expected_size, digest, expected_mtime_seconds))
        else {
            continue;
        };
        // Ambiguity is not an integrity failure: retain each independently verified upload.
        if local_candidates.len() != 1 || remote_candidates.len() != 1 {
            continue;
        }

        let upload_index = local_candidates[0];
        let upload = &plan.uploads[upload_index];
        let source = remote_candidates[0];
        let (source_parent, source_name) = crate::path::parent_and_name(&source.remote_path)?;
        let (target_parent, target_name) = crate::path::parent_and_name(&upload.remote_path)?;
        // CopyMove can copy without removing the source, but it cannot assign a new basename.
        // Only optimize a parent change whose final name is already exact; every other rename
        // safely keeps the verified-upload path.
        if source_parent == target_parent || source_name != target_name {
            continue;
        }
        replacements.insert(
            upload_index,
            CopyAction {
                from_relative: source.relative.clone(),
                from_remote_path: source.remote_path.clone(),
                to_relative: upload.local.relative.clone(),
                to_remote_path: upload.remote_path.clone(),
                local: upload.local.clone(),
                expected_size,
                content_md5: digest,
                source_snapshot: RemoteSnapshot {
                    kind: EntryKind::File,
                    size: source.size,
                    mtime_seconds: source.mtime_seconds,
                    content_md5: Some(digest),
                    require_mtime: true,
                },
            },
        );
    }

    if replacements.is_empty() {
        return Ok(());
    }
    let mut kept = Vec::with_capacity(plan.uploads.len() - replacements.len());
    for (index, upload) in plan.uploads.drain(..).enumerate() {
        if let Some(copy) = replacements.remove(&index) {
            plan.upload_bytes = plan.upload_bytes.saturating_sub(upload.local.size);
            plan.copies.push(copy);
        } else {
            kept.push(upload);
        }
    }
    plan.uploads = kept;
    Ok(())
}

fn remote_subtree<'a>(
    entries: &'a std::collections::BTreeMap<String, RemoteEntry>,
    relative: &str,
) -> Vec<&'a RemoteEntry> {
    // Range queries on the ordered map: the entry itself, then its descendants, which sort
    // contiguously after `relative/`. Filtering every entry instead cost the whole inventory per
    // type-conflict candidate.
    let prefix = format!("{relative}/");
    entries
        .get(relative)
        .into_iter()
        .chain(
            entries
                .range::<str, _>((
                    std::ops::Bound::Included(prefix.as_str()),
                    std::ops::Bound::Unbounded,
                ))
                .take_while(|(candidate, _)| candidate.starts_with(&prefix))
                .map(|(_, entry)| entry),
        )
        .collect()
}

fn is_protected(entry: &RemoteEntry, rules: &IgnoreRules) -> bool {
    entry.mount_point_type.is_some()
        || is_dsm_managed(&entry.relative)
        || rules.is_ignored(&entry.relative, entry.kind == EntryKind::Directory)
}

fn local_mtime_seconds(local: &LocalEntry) -> i64 {
    local.mtime_ms.div_euclid(1000)
}

/// Whether a local modification time and a File Station one describe the same moment, given the
/// precision each side actually carries.
///
/// File Station reports whole seconds. The local side carries milliseconds, which an upload sends
/// as they are. Whether DSM floors or rounds them is unverified (audit U-1). So this asks whether the
/// remote second is what DSM would have stored for this local time under either convention:
///
/// * the local time has a non-zero sub-second part: it agrees with its floor (DSM truncates) or its
///   half-up rounding (DSM rounds) and with nothing else. `3.2 s` agrees with `3` only, `3.7 s` with
///   `3` or `4`;
/// * the local time is a whole second (no sub-second part, as on FAT, many SMB servers, or a time
///   that itself came back from File Station): floor and round coincide, so only that exact second
///   agrees. Whole-second against whole-second one second apart is a change.
///
/// Anything beyond that granularity counts as changed. A blanket +/-1 s tolerance was tried and
/// rejected: it made every same-size edit that moved the mtime by up to a second invisible in
/// `--compare metadata`.
///
/// The residual is inherent, not a choice: an edit that keeps the size and lands in the same second
/// (or the same rounding bucket) as the previous version is undetectable in `--compare metadata`,
/// because File Station cannot say anything finer. `--compare content` compares digests for every
/// pair this accepts, because `select_comparison_remote_digests` applies this same test.
///
/// Copy-candidate matching stays exact on purpose -- it authorises a server copy and a guarded
/// delete -- and remote-to-remote snapshot checks never use this.
///
/// The one definition every local-to-remote comparison uses -- the planner's re-upload decision,
/// its digest selection, the post-upload "did DSM keep the time" latch and the executor's
/// destination checks -- so no two of them can disagree about whether a file changed.
pub fn mtimes_agree(local_mtime_ms: i64, remote_mtime_seconds: i64) -> bool {
    let floored = local_mtime_ms.div_euclid(1000);
    let rounded = local_mtime_ms.saturating_add(500).div_euclid(1000);
    remote_mtime_seconds == floored || remote_mtime_seconds == rounded
}

fn deletion_snapshot(
    entry: &RemoteEntry,
    remote_entries: &BTreeMap<String, RemoteEntry>,
    compare: CompareMode,
) -> Result<RemoteSnapshot> {
    // An empty file has exactly one possible content, so the size guard the executor re-checks
    // before deleting already proves it. Requiring a digest on top meant one DSM hash task per empty
    // remote file, and a mirror run that failed outright if DSM declines to hash an empty file.
    let content_md5 = if compare.requires_deletion_digest()
        && entry.kind == EntryKind::File
        && entry.size > 0
    {
        let fingerprint = entry.content_md5.ok_or_else(|| {
            Error::Message(format!(
                "content-mode deletion requires a plan-time MD5/CRC32/SHA-256 fingerprint for {:?}",
                entry.remote_path
            ))
        })?;
        if !fingerprint.has_full_proof() {
            return Err(Error::Message(format!(
                "content-mode deletion requires a complete plan-time MD5/CRC32/SHA-256 fingerprint for {:?}",
                entry.remote_path
            )));
        }
        Some(fingerprint)
    } else {
        None
    };
    // A range query: descendants sort contiguously after `relative/`, so the first key at or after
    // the prefix answers the question. Scanning every key -- with a fresh `format!` per key -- made
    // a first mirror run with D remote-only directories cost D x N allocations.
    let has_descendants = entry.kind == EntryKind::Directory && {
        let prefix = format!("{}/", entry.relative);
        remote_entries
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .is_some_and(|(relative, _)| relative.starts_with(&prefix))
    };
    Ok(RemoteSnapshot {
        kind: entry.kind,
        size: entry.size,
        mtime_seconds: entry.mtime_seconds,
        content_md5,
        require_mtime: !has_descendants,
    })
}

fn relative_parent_and_name(relative: &str) -> (&str, &str) {
    relative.rsplit_once('/').unwrap_or(("", relative))
}

fn has_local_file_ancestor(relative: &str, local: &LocalInventory) -> bool {
    let mut current = relative;
    while let Some((parent, _)) = current.rsplit_once('/') {
        if local
            .entries
            .get(parent)
            .is_some_and(|entry| entry.kind == EntryKind::File)
        {
            return true;
        }
        current = parent;
    }
    false
}

fn add_ancestor_directories(relative: &str, output: &mut HashSet<String>) {
    let mut current = relative;
    while let Some((parent, _)) = current.rsplit_once('/') {
        output.insert(parent.to_owned());
        current = parent;
    }
}

fn sort_plan(plan: &mut SyncPlan) {
    plan.pre_deletes.sort_by(|left, right| {
        depth(&right.relative)
            .cmp(&depth(&left.relative))
            .then_with(|| left.relative.cmp(&right.relative))
    });
    plan.creates.sort_by(|left, right| {
        depth(&left.relative)
            .cmp(&depth(&right.relative))
            .then_with(|| left.relative.cmp(&right.relative))
    });
    plan.copies
        .sort_by(|left, right| left.to_relative.cmp(&right.to_relative));
    plan.uploads
        .sort_by(|left, right| left.local.relative.cmp(&right.local.relative));
    plan.post_deletes.sort_by(|left, right| {
        depth(&right.relative)
            .cmp(&depth(&left.relative))
            .then_with(|| left.relative.cmp(&right.relative))
    });
}

// ---------------------------------------------------------------------------
// Scoped status query
//
// A live query, never an index. Nothing here is written to disk and nothing is cached between
// calls: the answer is rebuilt from the two inventories on every request, exactly as `build_plan`
// rebuilds content correspondence on every run. There is therefore nothing to go stale.
//
// Per-entry outcomes come from `compare_files`, the same function `build_plan` uses, so a status
// row can never disagree with what a sync would actually do.
// ---------------------------------------------------------------------------

/// Default rows per page when a caller does not choose.
pub const STATUS_PAGE_SIZE_DEFAULT: usize = 100;

/// Hard ceiling on rows per page.
///
/// Enforced in [`build_status_page`] rather than in an argument parser, so no caller on any
/// surface can obtain a larger page. The target hardware is armv7; a page is a per-request
/// allocation on a device that cannot absorb an unbounded one.
pub const STATUS_PAGE_SIZE_MAX: usize = 200;

/// Why an entry is excluded from synchronization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusionCause {
    /// Matched an ignore rule or `--exclude` pattern.
    IgnoreRule,
    /// A DSM administrative directory, which is never payload.
    DsmManaged,
    /// A File Station mount point. Its contents belong to another filesystem.
    MountBoundary,
}

impl ExclusionCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IgnoreRule => "ignore-rule",
            Self::DsmManaged => "dsm-managed",
            Self::MountBoundary => "mount-boundary",
        }
    }
}

/// The comparison outcome for one entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusState {
    /// Present on both sides and equal under the active compare mode.
    InSync,
    /// Present on both sides and scheduled for upload. Carries the planner's own reason.
    Differs(ChangeReason),
    /// Present locally, absent remotely.
    MissingRemote,
    /// Present remotely, absent locally. Removed only by a mirror run.
    RemoteOnly,
    /// Both sides exist but disagree about file versus directory.
    TypeConflict {
        local_kind: EntryKind,
        remote_kind: EntryKind,
    },
    /// Never synchronized, and why.
    Excluded(ExclusionCause),
    /// Present on one side, and the other side's walk stopped before it could say.
    ///
    /// Absence is what decides [`Self::RemoteOnly`] and [`Self::MissingRemote`], and a walk the scan
    /// budget truncated cannot tell an entry it never reached from one that does not exist. The two
    /// walks truncate at different points — the local one is a sorted depth-first recursion, the
    /// remote one a LIFO stack — so past the budget the two sets diverge structurally rather than
    /// agreeing on a prefix.
    ///
    /// Reported as its own state because the alternative was worse than silence: six identical
    /// in-sync pairs with the local side truncated to three produced three `remote-only` rows, and
    /// `RemoteOnly` documents itself as *"Present remotely, absent locally. Removed only by a mirror
    /// run"* — so the dashboard told the operator a mirror run would delete files that were present
    /// locally and identical, and counted all three as needing attention. Truncation makes
    /// `in_sync_files` a floor and `remote_only_entries` and `missing_remote_files` *over*counts,
    /// and one notice claiming every total was a lower bound was false for exactly the two it
    /// inflated.
    ///
    /// Not in [`StateKind::ATTENTION`]: nothing here needs a person to act, and putting it there
    /// would keep inflating the attention count this exists to stop inflating.
    NotCompared,
}

impl StatusState {
    pub fn kind(self) -> StateKind {
        match self {
            Self::InSync => StateKind::InSync,
            Self::Differs(_) => StateKind::Differs,
            Self::MissingRemote => StateKind::MissingRemote,
            Self::RemoteOnly => StateKind::RemoteOnly,
            Self::TypeConflict { .. } => StateKind::TypeConflict,
            Self::Excluded(_) => StateKind::Excluded,
            Self::NotCompared => StateKind::NotCompared,
        }
    }

    /// The planner's reason, when one applies.
    pub fn reason(self) -> Option<ChangeReason> {
        match self {
            Self::Differs(reason) => Some(reason),
            Self::MissingRemote => Some(ChangeReason::MissingRemote),
            _ => None,
        }
    }
}

/// The filterable name of a [`StatusState`], without its payload.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StateKind {
    TypeConflict,
    MissingRemote,
    Differs,
    RemoteOnly,
    InSync,
    Excluded,
    /// One side's walk was truncated by the scan budget before it could say.
    NotCompared,
}

impl StateKind {
    /// The states that need a person's attention, newest concern first.
    ///
    /// This is the canonical set behind the `attention` filter. It is defined once, here, so that
    /// a caller selecting the default view cannot hand-roll a set that drifts from this one.
    ///
    /// `TypeConflict` is included even though it is not a difference: it is the one state that
    /// makes a sync *fail* rather than proceed, so a view that hid it would hide the thing most
    /// likely to be blocking the user.
    pub const ATTENTION: &'static [Self] = &[
        Self::TypeConflict,
        Self::MissingRemote,
        Self::Differs,
        Self::RemoteOnly,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TypeConflict => "type-conflict",
            Self::MissingRemote => "missing-remote",
            Self::Differs => "differs",
            Self::RemoteOnly => "remote-only",
            Self::InSync => "in-sync",
            Self::Excluded => "excluded",
            Self::NotCompared => "not-compared",
        }
    }

    pub fn needs_attention(self) -> bool {
        Self::ATTENTION.contains(&self)
    }
}

/// Which states a listing returns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateFilter {
    /// Every state.
    All,
    /// Only the named states. [`StateKind::ATTENTION`] is the set behind the default UI view.
    Only(Vec<StateKind>),
}

impl Default for StateFilter {
    /// Items needing attention, which is the view a caller wants unless it says otherwise.
    ///
    /// The command line opts into [`Self::All`] explicitly instead: an exhaustive listing is the
    /// better default for a shell, where the caller sees the whole answer at once.
    fn default() -> Self {
        Self::attention()
    }
}

impl StateFilter {
    /// The set a caller showing "only items needing attention" should ask for.
    pub fn attention() -> Self {
        Self::Only(StateKind::ATTENTION.to_vec())
    }

    fn admits(&self, kind: StateKind) -> bool {
        match self {
            Self::All => true,
            Self::Only(kinds) => kinds.contains(&kind),
        }
    }
}

/// One side's metadata for a compared entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SideInfo {
    pub size: u64,
    pub mtime_seconds: i64,
}

/// One row of a status listing.
///
/// Deliberately small: a caller renders a full page of these without a follow-up request per row.
/// Whether an entry would transfer, and how many bytes, is derivable from `state` and `local`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusEntry {
    /// Source-relative path. The stable identity a caller passes back to scope an action.
    pub relative: String,
    /// Full File Station path, so a caller never has to join paths itself.
    pub remote_path: String,
    pub kind: EntryKind,
    pub state: StatusState,
    pub local: Option<SideInfo>,
    pub remote: Option<SideInfo>,
}

/// Whole-scope totals.
///
/// These describe the entire scope, not the returned page, and filters never narrow them. That is
/// what lets a caller showing 200 attention rows still say how many files are in sync behind them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusStats {
    /// The comparison the answer was actually produced with.
    ///
    /// Echoed so a caller can label how strict its own answer is. A view that deliberately asks
    /// for a cheap comparison over a broad scope is being economical; one that cannot tell which
    /// comparison it received is guessing.
    pub compare: CompareMode,
    pub in_sync_files: usize,
    pub differing_files: usize,
    pub missing_remote_files: usize,
    pub remote_only_entries: usize,
    pub type_conflicts: usize,
    pub excluded_entries: usize,
    /// Entries neither walk could compare, because the scan budget truncated the other side.
    ///
    /// Non-zero only when `complete` is false. While it is non-zero, `in_sync_files`,
    /// `differing_files` and `type_conflicts` are floors and `remote_only_entries` and
    /// `missing_remote_files` are unusable rather than merely low: presence alone cannot decide
    /// either of them.
    pub not_compared_entries: usize,
    pub directories: usize,
    pub in_sync_bytes: u64,
    /// Bytes that would transfer, accumulated exactly as `SyncPlan::upload_bytes` is.
    pub transfer_bytes: u64,
    pub total_entries: usize,
    /// Entries in [`StateKind::ATTENTION`]; the row count of the default view.
    pub attention_entries: usize,
    /// False when a scan budget stopped the walk, making every count a lower bound.
    pub complete: bool,
}

impl StatusStats {
    /// Zeroed totals for `compare`.
    ///
    /// Public so a consumer that persists or renders these can construct the same shape the engine
    /// produces, rather than inventing a parallel one whose fields could drift from these.
    pub fn empty(compare: CompareMode) -> Self {
        Self::new(compare)
    }

    fn new(compare: CompareMode) -> Self {
        Self {
            compare,
            in_sync_files: 0,
            differing_files: 0,
            missing_remote_files: 0,
            remote_only_entries: 0,
            type_conflicts: 0,
            excluded_entries: 0,
            not_compared_entries: 0,
            directories: 0,
            in_sync_bytes: 0,
            transfer_bytes: 0,
            total_entries: 0,
            attention_entries: 0,
            complete: true,
        }
    }
}

/// A position in a listing. Not a snapshot, and not stored anywhere.
///
/// Resuming re-derives the answer from live state, so a cursor naming a path that has since been
/// deleted still resumes correctly: the comparison is ordering, not lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusCursor(String);

impl StatusCursor {
    pub fn new(relative: impl Into<String>) -> Self {
        Self(relative.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A bounded page plus the totals for the whole scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusPage {
    pub scope: String,
    pub entries: Vec<StatusEntry>,
    /// The limit actually applied, after clamping to [`STATUS_PAGE_SIZE_MAX`].
    pub limit: usize,
    pub next_cursor: Option<StatusCursor>,
    pub truncated: bool,
    pub stats: StatusStats,
}

/// Inputs to one status listing.
#[derive(Clone, Debug)]
pub struct StatusQuery<'a> {
    pub scope: &'a Scope,
    pub compare: CompareMode,
    /// Case-insensitive substring matched against the final path component.
    pub filter: Option<&'a str>,
    pub states: StateFilter,
    pub include_excluded: bool,
    pub limit: usize,
    pub cursor: Option<&'a StatusCursor>,
    /// Whether the local walk saw the whole scope, or stopped at the scan budget.
    ///
    /// When it did not, absence on the local side proves nothing, so an entry present only remotely
    /// is [`StatusState::NotCompared`] rather than [`StatusState::RemoteOnly`].
    pub local_complete: bool,
    /// The same, for the remote walk and [`StatusState::MissingRemote`].
    pub remote_complete: bool,
}

/// Answer a scoped status query from live inventories.
///
/// `excluded` carries paths the scoped scan pruned; it is empty unless the caller asked for them.
/// Ordering is lexicographic over the merged local/remote key set, which makes the cursor a plain
/// path and paging stateless.
pub fn build_status_page(
    root: &RemoteRoot,
    local: &LocalInventory,
    remote: &RemoteInventory,
    excluded: &BTreeSet<String>,
    rules: &IgnoreRules,
    query: &StatusQuery<'_>,
) -> Result<StatusPage> {
    let limit = query.limit.clamp(1, STATUS_PAGE_SIZE_MAX);

    // Rows are keyed by the local spelling. A remote entry that is the same name under another
    // Unicode normalisation is that row's remote side, exactly as `build_plan` pairs it, rather
    // than a second row claiming the file is missing on one side and extra on the other.
    let pairs = Correspondence::build(local, remote);
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for relative in local.entries.keys() {
        if query.scope.matches(relative) {
            keys.insert(relative.as_str());
        }
    }
    for relative in remote.entries.keys() {
        if query.scope.matches(relative) && !pairs.is_paired_remote(relative) {
            keys.insert(relative.as_str());
        }
    }
    if query.include_excluded {
        for relative in excluded {
            if query.scope.matches(relative) {
                keys.insert(relative.as_str());
            }
        }
    }

    let mut stats = StatusStats::new(query.compare);
    let mut entries = Vec::new();
    let mut next_cursor = None;
    let after = query.cursor.map(StatusCursor::as_str);

    for relative in keys {
        let local_entry = local.entries.get(relative);
        let remote_entry = match local_entry {
            Some(_) => pairs.remote_of(remote, relative),
            None => remote.entries.get(relative),
        };
        let entry = classify(
            root,
            local_entry,
            remote_entry,
            excluded,
            rules,
            query,
            relative,
        )?;
        accumulate(&mut stats, &entry);

        if !query.states.admits(entry.state.kind()) || !matches_filter(relative, query.filter) {
            continue;
        }
        if after.is_some_and(|cursor| relative <= cursor) {
            continue;
        }
        if entries.len() == limit {
            // The page is full, but the walk continues so the stats still describe the whole
            // scope. The first row that does not fit fixes the cursor for the next page.
            if next_cursor.is_none() {
                next_cursor = entries
                    .last()
                    .map(|last: &StatusEntry| StatusCursor::new(last.relative.clone()));
            }
            continue;
        }
        entries.push(entry);
    }

    Ok(StatusPage {
        scope: query.scope.as_str().to_owned(),
        entries,
        limit,
        truncated: next_cursor.is_some(),
        next_cursor,
        stats,
    })
}

/// Length of a resync ticket, in hex characters.
const RESYNC_TICKET_LENGTH: usize = 16;

/// Where a forced re-upload would write, as the ticket describes it.
///
/// Both halves are part of the confirmation because both decide which bytes are destroyed. A
/// ticket that covered only the local overwrite set would be replayable against any destination
/// reachable with the same source, which is the one thing a confirmation exists to prevent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResyncTarget<'a> {
    /// The destination root, as the remote path it maps to.
    pub remote_root: &'a str,
    /// The DSM endpoint the root lives on, normalized as the client received it.
    pub endpoint: &'a str,
}

/// Derive the ticket that identifies exactly what a forced re-upload would overwrite.
///
/// A confirmation has to prove the caller saw *this* set of files on *this* destination, not
/// merely that the command was run twice. The ticket is a digest over the endpoint, the remote
/// root, the scope, the comparison, the sorted list of paths that would be overwritten, and the
/// byte total — so any change to what is about to happen produces a different ticket and the
/// confirmation is refused.
///
/// The endpoint and the root are what made version 2 of the encoding. Under version 1 the upload
/// set was a function of local state alone (in `Force` mode every in-scope file is scheduled
/// without consulting the destination), so two profiles pointing one source at two different
/// destinations — or at two different NAS units — produced byte-identical tickets, and a ticket
/// printed for one confirmed an overwrite of the other.
///
/// Each upload's **content digest** is what makes version 3, and it closes the scenario the other two
/// left open: the same destination, the same path, the same length, different bytes. The operator is
/// shown "overwrite `summary.pdf`, 1,024,000 bytes" and given a ticket; before confirming, the file
/// is replaced by a regenerated report, a restored older revision, or the wrong file dropped in by a
/// colleague, at the same length. The ticket still matched, and `resync` — the one command that
/// destroys remote revisions unconditionally — overwrote the remote copy with content nobody
/// reviewed. Both `RESYNC_LONG_ABOUT` and `--confirm`'s help say a ticket stops matching "if either
/// side has changed since", which was true of every property except the bytes. The digest is already
/// in hand at ticket time: `promote_upload_fingerprints` runs before the plan guard, and in `Force`
/// mode it is what computes the digest at all.
///
/// The domain tag is part of the hashed string, so a version 1 or 2 ticket simply fails to match and
/// is reported stale with the refreshed plan and its new ticket.
///
/// Deliberately carries no clock. Nothing here is stored, so there is no expiry to enforce and no
/// business inventing one: a ticket stops being valid when what it describes changes, and stays
/// valid indefinitely when it does not.
pub fn resync_ticket(
    target: ResyncTarget<'_>,
    scope: &Scope,
    compare: CompareMode,
    plan: &SyncPlan,
) -> String {
    let mut canonical = String::from("sdsync.resync-ticket.v3\n");
    canonical.push_str(target.endpoint);
    canonical.push('\n');
    canonical.push_str(target.remote_root);
    canonical.push('\n');
    canonical.push_str(scope.as_str());
    canonical.push('\n');
    canonical.push_str(match compare {
        CompareMode::Content => "content",
        CompareMode::Metadata => "metadata",
        CompareMode::SizeOnly => "size-only",
        CompareMode::Force => "force",
    });
    canonical.push('\n');
    canonical.push_str(&plan.upload_bytes.to_string());
    canonical.push('\n');
    // `sort_plan` already orders uploads by relative path, so the encoding is stable across runs
    // that would do the same work.
    for action in &plan.uploads {
        canonical.push_str(&action.local.relative);
        canonical.push('\0');
        canonical.push_str(&action.local.size.to_string());
        canonical.push('\0');
        // The SHA-256 of the bytes that would be written, when one is in hand. A digest-less upload
        // encodes as "none", which is a *different* string from any digest rather than an absence
        // that could be forged into one -- and on the `resync` path it cannot occur, because the
        // promotion that computes it runs before the guard that calls this.
        canonical.push_str(
            &action
                .local
                .content_md5
                .and_then(|digest| digest.sha256_hex())
                .unwrap_or_else(|| "none".to_owned()),
        );
        canonical.push('\n');
    }

    let mut ticket = ContentMd5::from_content(canonical.as_bytes())
        .sha256_hex()
        .expect("a freshly computed fingerprint carries its SHA-256");
    ticket.truncate(RESYNC_TICKET_LENGTH);
    ticket
}

fn matches_filter(relative: &str, filter: Option<&str>) -> bool {
    let Some(filter) = filter.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let (_, name) = relative_parent_and_name(relative);
    name.to_lowercase().contains(&filter.to_lowercase())
}

fn accumulate(stats: &mut StatusStats, entry: &StatusEntry) {
    stats.total_entries += 1;
    if entry.state.kind().needs_attention() {
        stats.attention_entries += 1;
    }
    if entry.kind == EntryKind::Directory && !matches!(entry.state, StatusState::Excluded(_)) {
        stats.directories += 1;
    }
    let size = entry.local.map_or(0, |side| side.size);
    match entry.state {
        StatusState::InSync => {
            if entry.kind == EntryKind::File {
                stats.in_sync_files += 1;
                stats.in_sync_bytes = stats.in_sync_bytes.saturating_add(size);
            }
        }
        StatusState::Differs(_) => {
            stats.differing_files += 1;
            stats.transfer_bytes = stats.transfer_bytes.saturating_add(size);
        }
        StatusState::MissingRemote => {
            if entry.kind == EntryKind::File {
                stats.missing_remote_files += 1;
                stats.transfer_bytes = stats.transfer_bytes.saturating_add(size);
            }
        }
        StatusState::RemoteOnly => stats.remote_only_entries += 1,
        StatusState::TypeConflict { .. } => {
            stats.type_conflicts += 1;
            stats.transfer_bytes = stats.transfer_bytes.saturating_add(size);
        }
        StatusState::Excluded(_) => stats.excluded_entries += 1,
        StatusState::NotCompared => stats.not_compared_entries += 1,
    }
}

fn classify(
    root: &RemoteRoot,
    local_entry: Option<&LocalEntry>,
    remote_entry: Option<&RemoteEntry>,
    excluded: &BTreeSet<String>,
    rules: &IgnoreRules,
    query: &StatusQuery<'_>,
    relative: &str,
) -> Result<StatusEntry> {
    let compare = query.compare;

    let kind = local_entry
        .map(|entry| entry.kind)
        .or_else(|| remote_entry.map(|entry| entry.kind))
        .unwrap_or(EntryKind::Directory);

    let remote_path = match remote_entry {
        Some(entry) => entry.remote_path.clone(),
        None => root.join(relative)?,
    };

    let state = if let Some(entry) = remote_entry.filter(|entry| is_protected(entry, rules)) {
        StatusState::Excluded(if entry.mount_point_type.is_some() {
            ExclusionCause::MountBoundary
        } else if is_dsm_managed(&entry.relative) {
            ExclusionCause::DsmManaged
        } else {
            ExclusionCause::IgnoreRule
        })
    } else if local_entry.is_none() && remote_entry.is_none() && excluded.contains(relative) {
        StatusState::Excluded(if is_dsm_managed(relative) {
            ExclusionCause::DsmManaged
        } else {
            ExclusionCause::IgnoreRule
        })
    } else {
        match (local_entry, remote_entry) {
            (Some(local_entry), Some(remote_entry)) => {
                match (local_entry.kind, remote_entry.kind) {
                    (EntryKind::Directory, EntryKind::Directory) => StatusState::InSync,
                    (EntryKind::File, EntryKind::File) => {
                        match compare_files(local_entry, remote_entry, compare) {
                            None => StatusState::InSync,
                            Some(reason) => StatusState::Differs(reason),
                        }
                    }
                    (local_kind, remote_kind) => StatusState::TypeConflict {
                        local_kind,
                        remote_kind,
                    },
                }
            }
            // Absence is only evidence when the walk that would have found it finished. A budget
            // that stopped the remote walk cannot distinguish "not on the NAS" from "not reached".
            (Some(_), None) if !query.remote_complete => StatusState::NotCompared,
            (None, Some(_)) if !query.local_complete => StatusState::NotCompared,
            (Some(_), None) => StatusState::MissingRemote,
            (None, Some(_)) => StatusState::RemoteOnly,
            (None, None) => StatusState::Excluded(ExclusionCause::IgnoreRule),
        }
    };

    Ok(StatusEntry {
        relative: relative.to_owned(),
        remote_path,
        kind,
        state,
        local: local_entry.map(|entry| SideInfo {
            size: entry.size,
            mtime_seconds: local_mtime_seconds(entry),
        }),
        remote: remote_entry.map(|entry| SideInfo {
            size: entry.size,
            mtime_seconds: entry.mtime_seconds,
        }),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn local(entries: &[(&str, EntryKind, u64, i64)]) -> LocalInventory {
        LocalInventory {
            root: PathBuf::from("/source"),
            entries: entries
                .iter()
                .map(|(relative, kind, size, mtime_ms)| {
                    (
                        (*relative).to_owned(),
                        LocalEntry {
                            relative: (*relative).to_owned(),
                            full_path: PathBuf::from(relative),
                            kind: *kind,
                            size: *size,
                            mtime_ms: *mtime_ms,
                            identity: Default::default(),
                            content_md5: None,
                        },
                    )
                })
                .collect(),
            skipped: Vec::new(),
        }
    }

    fn remote(entries: &[(&str, EntryKind, u64, i64)]) -> RemoteInventory {
        RemoteInventory {
            root_exists: true,
            entries: entries
                .iter()
                .map(|(relative, kind, size, mtime_seconds)| {
                    (
                        (*relative).to_owned(),
                        RemoteEntry {
                            relative: (*relative).to_owned(),
                            remote_path: format!("/share/root/{relative}"),
                            kind: *kind,
                            size: *size,
                            mtime_seconds: *mtime_seconds,
                            mount_point_type: None,
                            content_md5: None,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn rules(patterns: &[&str]) -> IgnoreRules {
        let root = std::env::temp_dir().join(format!("sdsync-plan-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        IgnoreRules::build(
            &root,
            &patterns
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn options(delete: bool) -> PlanOptions {
        PlanOptions {
            delete,
            allow_empty_source: false,
            max_delete: 100,
            compare: CompareMode::Metadata,
            server_copy: false,
            scope: Scope::root(),
        }
    }

    fn digest(value: u8) -> ContentMd5 {
        ContentMd5::from_digests([value; 16], u32::from(value), [value; 32])
    }

    fn content_options(delete: bool, server_copy: bool) -> PlanOptions {
        PlanOptions {
            compare: CompareMode::Content,
            server_copy,
            scope: Scope::root(),
            ..options(delete)
        }
    }

    #[test]
    fn plans_create_upload_change_and_unchanged() {
        let local = local(&[
            ("empty", EntryKind::Directory, 0, 0),
            ("new.txt", EntryKind::File, 3, 2_000),
            ("same.txt", EntryKind::File, 4, 3_500),
            ("changed.txt", EntryKind::File, 4, 5_000),
        ]);
        let remote = remote(&[
            ("same.txt", EntryKind::File, 4, 3),
            ("changed.txt", EntryKind::File, 4, 4),
            ("extra.txt", EntryKind::File, 1, 1),
        ]);
        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &options(false),
        )
        .unwrap();
        assert_eq!(plan.creates.len(), 1);
        assert_eq!(plan.uploads.len(), 2);
        assert_eq!(plan.unchanged_files, 1);
        assert!(plan.post_deletes.is_empty());
        assert_eq!(plan.creates[0].reason, ChangeReason::MissingRemote);
        assert_eq!(
            plan.uploads
                .iter()
                .map(|action| (action.local.relative.as_str(), action.reason))
                .collect::<Vec<_>>(),
            [
                ("changed.txt", ChangeReason::MtimeDiffers),
                ("new.txt", ChangeReason::MissingRemote),
            ]
        );
    }

    #[test]
    fn change_reasons_name_only_the_comparison_each_mode_performs() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let grown = local(&[("payload.bin", EntryKind::File, 5, 1_000)]);
        let touched = local(&[("payload.bin", EntryKind::File, 4, 9_000)]);
        let remote_entry = remote(&[("payload.bin", EntryKind::File, 4, 1)]);

        for mode in [
            options(false),
            PlanOptions {
                compare: CompareMode::SizeOnly,
                ..options(false)
            },
            content_options(false, false),
        ] {
            let plan = build_plan(&root, &grown, &remote_entry, &rules(&[]), &mode).unwrap();
            assert_eq!(
                plan.uploads[0].reason,
                ChangeReason::SizeDiffers,
                "a size difference outranks every other comparison in {:?} mode",
                mode.compare
            );
        }

        // Size-only never inspects a timestamp, so an mtime change is not a change at all.
        let size_only = PlanOptions {
            compare: CompareMode::SizeOnly,
            ..options(false)
        };
        let unchanged =
            build_plan(&root, &touched, &remote_entry, &rules(&[]), &size_only).unwrap();
        assert!(unchanged.uploads.is_empty());
        assert_eq!(unchanged.unchanged_files, 1);

        let metadata =
            build_plan(&root, &touched, &remote_entry, &rules(&[]), &options(false)).unwrap();
        assert_eq!(metadata.uploads[0].reason, ChangeReason::MtimeDiffers);
    }

    #[test]
    fn content_mode_names_the_missing_digest_instead_of_a_comparison_it_never_made() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        // Metadata identical, so only a digest pair could decide equality.
        let matched_remote = || remote(&[("payload.bin", EntryKind::File, 4, 1)]);
        let plan_for = |local: &LocalInventory, remote: &RemoteInventory| {
            build_plan(
                &root,
                local,
                remote,
                &rules(&[]),
                &content_options(false, false),
            )
            .unwrap()
        };

        // Remote digests are fetched only for equal size and mtime, so a same-size mtime change
        // has no digest pair to compare and must not be reported as a content difference.
        let mut moved = local(&[("payload.bin", EntryKind::File, 4, 9_000)]);
        moved.entries.get_mut("payload.bin").unwrap().content_md5 = Some(digest(1));
        assert_eq!(
            plan_for(&moved, &matched_remote()).uploads[0].reason,
            ChangeReason::MtimeDiffers
        );

        // A local file left unhashed - hashing skipped or cancelled - was never compared, so the
        // conservative upload must say the local fingerprint was missing, not that the hashes disagreed.
        let unhashed = local(&[("payload.bin", EntryKind::File, 4, 1_000)]);
        let mut hashed_remote = matched_remote();
        hashed_remote
            .entries
            .get_mut("payload.bin")
            .unwrap()
            .content_md5 = Some(digest(1));
        assert_eq!(
            plan_for(&unhashed, &hashed_remote).uploads[0].reason,
            ChangeReason::LocalDigestUnavailable
        );

        // Neither side hashed: the local digest is the precondition for the remote lookup, so it
        // is the side named.
        assert_eq!(
            plan_for(&unhashed, &matched_remote()).uploads[0].reason,
            ChangeReason::LocalDigestUnavailable
        );

        // The mirror image: the local file was hashed but the remote entry carries no fingerprint,
        // as happens when digest selection skipped it - a protected path, say.
        let mut hashed = local(&[("payload.bin", EntryKind::File, 4, 1_000)]);
        hashed.entries.get_mut("payload.bin").unwrap().content_md5 = Some(digest(1));
        assert_eq!(
            plan_for(&hashed, &matched_remote()).uploads[0].reason,
            ChangeReason::RemoteDigestUnavailable
        );

        // The regression guard that matters: two present, differing digests are still a genuine
        // content difference.
        let mut differing_remote = matched_remote();
        differing_remote
            .entries
            .get_mut("payload.bin")
            .unwrap()
            .content_md5 = Some(digest(2));
        assert_eq!(
            plan_for(&hashed, &differing_remote).uploads[0].reason,
            ChangeReason::ContentDiffers
        );

        // Two present, equal digests remain no change at all.
        let unchanged = plan_for(&hashed, &hashed_remote);
        assert!(unchanged.uploads.is_empty());
        assert_eq!(unchanged.unchanged_files, 1);
    }

    #[test]
    fn unverified_content_details_report_the_missing_digest_not_a_mismatch() {
        assert_eq!(
            ChangeReason::LocalDigestUnavailable.detail(),
            "size and time equal, no complete local MD5/CRC32/SHA-256 fingerprint, uploading unverified"
        );
        assert_eq!(
            ChangeReason::RemoteDigestUnavailable.detail(),
            "size and time equal, no complete remote MD5/CRC32/SHA-256 fingerprint, uploading unverified"
        );
        assert_eq!(
            ChangeReason::ContentDiffers.detail(),
            "size equal, complete MD5/CRC32/SHA-256 fingerprint did not match"
        );
    }

    #[test]
    fn content_mode_requires_crc32_and_sha256_even_when_md5_and_crc32_match() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let mut local = local(&[("payload.bin", EntryKind::File, 4, 1_000)]);
        let mut remote = remote(&[("payload.bin", EntryKind::File, 4, 1)]);
        let shared_md5 = [0x55; 16];
        let shared_crc32 = 0xaabb_ccdd;
        local.entries.get_mut("payload.bin").unwrap().content_md5 = Some(ContentMd5::from_digests(
            shared_md5,
            shared_crc32,
            [0x11; 32],
        ));
        remote.entries.get_mut("payload.bin").unwrap().content_md5 = Some(
            ContentMd5::from_digests(shared_md5, shared_crc32, [0x22; 32]),
        );

        let plan = build_plan(
            &root,
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, false),
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert_eq!(plan.uploads[0].reason, ChangeReason::ContentDiffers);
    }

    /// An MD5-only fingerprint on one side establishes equality, deliberately.
    ///
    /// This test asserted the opposite until content mode stopped downloading. A fingerprint
    /// without CRC32 and SHA-256 could not establish equality, which was the right rule while
    /// every remote digest came from reading the whole file back. Remote comparison digests now
    /// come from File Station's server-side MD5 — the only digest it will compute without
    /// transferring the file — so an MD5-only remote value is the *normal* case, and continuing to
    /// treat it as "cannot tell" would re-upload the entire tree on every scheduled run.
    ///
    /// The weakening is scoped to comparison, which only ever decides whether to upload. Digests
    /// that authorise a deletion or a server-side copy still require full strength; see
    /// `select_strong_remote_digests`.
    ///
    /// What must not change, and is the second half of this test: a difference is still a
    /// difference. An MD5 disagreement is reported as differing content whichever side is the
    /// MD5-only one.
    #[test]
    fn content_mode_accepts_an_md5_only_match_and_still_reports_an_md5_difference() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let legacy = ContentMd5::from_bytes([0x33; 16]);
        let complete = ContentMd5::from_digests([0x33; 16], 0x1122_3344, [0x44; 32]);
        let other_legacy = ContentMd5::from_bytes([0x99; 16]);
        let other_complete = ContentMd5::from_digests([0x99; 16], 0x5566_7788, [0xaa; 32]);
        let plan_for = |local_digest, remote_digest| {
            let mut local = local(&[("payload.bin", EntryKind::File, 4, 1_000)]);
            let mut remote = remote(&[("payload.bin", EntryKind::File, 4, 1)]);
            local.entries.get_mut("payload.bin").unwrap().content_md5 = Some(local_digest);
            remote.entries.get_mut("payload.bin").unwrap().content_md5 = Some(remote_digest);
            build_plan(
                &root,
                &local,
                &remote,
                &rules(&[]),
                &content_options(false, false),
            )
            .unwrap()
        };

        assert!(
            plan_for(complete, legacy).uploads.is_empty(),
            "a server-side MD5 agreeing with a complete local fingerprint means in sync"
        );
        assert!(
            plan_for(legacy, complete).uploads.is_empty(),
            "the same holds whichever side carries the strong digests"
        );

        assert_eq!(
            plan_for(complete, other_legacy).uploads[0].reason,
            ChangeReason::ContentDiffers
        );
        assert_eq!(
            plan_for(legacy, other_complete).uploads[0].reason,
            ChangeReason::ContentDiffers
        );
    }

    /// A digest that is absent entirely is still reported as absent, on the side that lacks it.
    ///
    /// Distinct from the MD5-only case above: "compared by MD5 alone" is an answer, while "no
    /// digest at all" is not one, and the two must not collapse into each other now that one of
    /// them has become the normal path. These reasons are part of the rendered output, so the
    /// distinction is visible to callers and not merely internal.
    #[test]
    fn content_mode_still_names_the_side_whose_digest_is_missing_entirely() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let complete = ContentMd5::from_digests([0x33; 16], 0x1122_3344, [0x44; 32]);
        let plan_for = |local_digest, remote_digest| {
            let mut local = local(&[("payload.bin", EntryKind::File, 4, 1_000)]);
            let mut remote = remote(&[("payload.bin", EntryKind::File, 4, 1)]);
            local.entries.get_mut("payload.bin").unwrap().content_md5 = local_digest;
            remote.entries.get_mut("payload.bin").unwrap().content_md5 = remote_digest;
            build_plan(
                &root,
                &local,
                &remote,
                &rules(&[]),
                &content_options(false, false),
            )
            .unwrap()
        };

        assert_eq!(
            plan_for(None, Some(complete)).uploads[0].reason,
            ChangeReason::LocalDigestUnavailable
        );
        assert_eq!(
            plan_for(Some(complete), None).uploads[0].reason,
            ChangeReason::RemoteDigestUnavailable
        );
    }

    #[test]
    fn type_replacement_is_reported_in_both_directions() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let file_over_directory = build_plan(
            &root,
            &local(&[("node", EntryKind::File, 1, 1_000)]),
            &remote(&[("node", EntryKind::Directory, 0, 0)]),
            &rules(&[]),
            &options(true),
        )
        .unwrap();
        assert_eq!(
            file_over_directory.uploads[0].reason,
            ChangeReason::TypeReplaced
        );

        // A source holding at least one file keeps the empty-source deletion guard out of the way.
        let directory_over_file = build_plan(
            &root,
            &local(&[
                ("keep.txt", EntryKind::File, 1, 1_000),
                ("node", EntryKind::Directory, 0, 0),
            ]),
            &remote(&[
                ("keep.txt", EntryKind::File, 1, 1),
                ("node", EntryKind::File, 1, 1),
            ]),
            &rules(&[]),
            &options(true),
        )
        .unwrap();
        assert_eq!(
            directory_over_file.creates[0].reason,
            ChangeReason::TypeReplaced
        );
    }

    #[test]
    fn every_change_reason_has_a_distinct_tag_and_explanation() {
        let reasons = [
            ChangeReason::MissingRemote,
            ChangeReason::SizeDiffers,
            ChangeReason::MtimeDiffers,
            ChangeReason::ContentDiffers,
            ChangeReason::LocalDigestUnavailable,
            ChangeReason::RemoteDigestUnavailable,
            ChangeReason::TypeReplaced,
        ];
        assert_eq!(
            reasons.map(ChangeReason::as_str),
            [
                "missing-remote",
                "size-differs",
                "mtime-differs",
                "content-differs",
                "local-digest-unavailable",
                "remote-digest-unavailable",
                "type-replaced",
            ]
        );
        assert_eq!(
            reasons
                .iter()
                .map(|reason| reason.detail())
                .collect::<BTreeSet<_>>()
                .len(),
            reasons.len()
        );
    }

    fn skips(plan: &SyncPlan) -> Vec<(&str, &'static str)> {
        plan.skipped
            .iter()
            .map(|skip| (skip.relative.as_str(), skip.reason.as_str()))
            .collect()
    }

    fn deleted(plan: &SyncPlan) -> Vec<&str> {
        plan.pre_deletes
            .iter()
            .chain(&plan.post_deletes)
            .map(|action| action.relative.as_str())
            .collect()
    }

    fn uploaded(plan: &SyncPlan) -> Vec<(&str, &str)> {
        plan.uploads
            .iter()
            .map(|action| (action.local.relative.as_str(), action.remote_path.as_str()))
            .collect()
    }

    /// N-2 without `--delete`: a case-only rename cannot be carried, because the old spelling may
    /// not be removed and the new one may not be written beside it. It is skipped and reported,
    /// with its subtree on both sides, and every other path still syncs. It used to fail the run.
    #[test]
    fn a_case_only_rename_without_delete_is_skipped_and_the_rest_still_syncs() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let local_inventory = local(&[
            ("folder", EntryKind::Directory, 0, 0),
            ("folder/local.bin", EntryKind::File, 1, 1_000),
            ("other.txt", EntryKind::File, 2, 2_000),
        ]);
        let remote_inventory = remote(&[
            ("Folder", EntryKind::Directory, 0, 0),
            ("Folder/remote.bin", EntryKind::File, 1, 1),
        ]);
        let plan = build_plan(
            &root,
            &local_inventory,
            &remote_inventory,
            &rules(&[]),
            &options(false),
        )
        .unwrap();
        assert_eq!(skips(&plan), [("folder", "case-rename-needs-delete")]);
        assert!(plan.skipped[0].reason.detail().contains("\"Folder\""));
        assert_eq!(
            uploaded(&plan),
            [("other.txt", "/share/root/other.txt")],
            "the renamed subtree is left alone and nothing else is held back"
        );
        assert!(plan.creates.is_empty());
    }

    /// N-2 under `--delete`: the rename converges as an upload of the new spelling and a deletion
    /// of the old one, for a file and for a directory with its contents.
    #[test]
    fn a_case_only_rename_under_delete_converges_as_upload_and_delete() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let plan = build_plan(
            &root,
            &local(&[
                ("report.pdf", EntryKind::File, 5, 5_000),
                ("photos", EntryKind::Directory, 0, 0),
                ("photos/a.jpg", EntryKind::File, 3, 3_000),
            ]),
            &remote(&[
                ("Report.pdf", EntryKind::File, 5, 5),
                ("Photos", EntryKind::Directory, 0, 0),
                ("Photos/a.jpg", EntryKind::File, 3, 3),
            ]),
            &rules(&[]),
            &options(true),
        )
        .unwrap();
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
        assert_eq!(
            plan.creates
                .iter()
                .map(|create| create.remote_path.as_str())
                .collect::<Vec<_>>(),
            ["/share/root/photos"]
        );
        assert_eq!(
            uploaded(&plan),
            [
                ("photos/a.jpg", "/share/root/photos/a.jpg"),
                ("report.pdf", "/share/root/report.pdf"),
            ]
        );
        assert_eq!(deleted(&plan), ["Photos/a.jpg", "Photos", "Report.pdf"]);
    }

    /// Two remote spellings of one name are a genuine collision. Neither is changed or deleted, the
    /// local spelling is not written beside them, both are named, and the rest of the tree syncs.
    #[test]
    fn a_remote_case_collision_is_skipped_on_both_sides_and_never_deleted() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        for delete in [false, true] {
            let plan = build_plan(
                &root,
                &local(&[
                    ("keep.txt", EntryKind::File, 1, 1_000),
                    ("Report.pdf", EntryKind::File, 9, 9_000),
                ]),
                &remote(&[
                    ("keep.txt", EntryKind::File, 1, 1),
                    ("Report.pdf", EntryKind::File, 1, 1),
                    ("report.pdf", EntryKind::File, 1, 1),
                    ("Archive", EntryKind::Directory, 0, 0),
                    ("archive", EntryKind::Directory, 0, 0),
                    ("stale.txt", EntryKind::File, 1, 1),
                ]),
                &rules(&[]),
                &options(delete),
            )
            .unwrap();
            assert_eq!(
                skips(&plan),
                [
                    ("Archive", "remote-case-collision"),
                    ("Report.pdf", "remote-case-collision"),
                    ("archive", "remote-case-collision"),
                    ("report.pdf", "remote-case-collision"),
                ]
            );
            assert!(
                plan.uploads.is_empty(),
                "the changed Report.pdf is held back"
            );
            if delete {
                assert_eq!(deleted(&plan), ["stale.txt"]);
            } else {
                assert!(deleted(&plan).is_empty());
            }
        }
    }

    /// Two local spellings are still never carried, whichever inventory reached the planner. The
    /// walk normally removes them first; the planner refuses them independently.
    #[test]
    fn a_local_case_collision_is_skipped_and_its_remote_spellings_are_never_deleted() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let plan = build_plan(
            &root,
            &local(&[
                ("Data.bin", EntryKind::File, 1, 1_000),
                ("data.bin", EntryKind::File, 2, 2_000),
                ("keep.txt", EntryKind::File, 1, 1_000),
            ]),
            &remote(&[
                ("DATA.BIN", EntryKind::File, 1, 1),
                ("keep.txt", EntryKind::File, 1, 1),
            ]),
            &rules(&[]),
            &options(true),
        )
        .unwrap();
        assert_eq!(
            skips(&plan),
            [
                ("Data.bin", "local-case-collision"),
                ("data.bin", "local-case-collision"),
            ]
        );
        assert!(plan.uploads.is_empty());
        assert!(deleted(&plan).is_empty());
    }

    /// N-1's safety half: a path the walk skipped exists locally, so neither it, nor anything
    /// beneath it, nor any case spelling of it is a mirror deletion candidate -- and its ancestors
    /// stay too, because they are not empty.
    #[test]
    fn paths_the_walk_skipped_are_never_deleted_and_are_reported() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let mut local_inventory = local(&[
            ("keep.txt", EntryKind::File, 1, 1_000),
            ("dir", EntryKind::Directory, 0, 0),
        ]);
        local_inventory.skipped = vec![
            SkippedEntry {
                relative: "Linked".to_owned(),
                reason: SkipReason::LinkOrReparsePoint,
            },
            SkippedEntry {
                relative: "dir/system.dat".to_owned(),
                reason: SkipReason::WindowsAttributes,
            },
        ];
        let remote_inventory = remote(&[
            ("keep.txt", EntryKind::File, 1, 1),
            ("Linked", EntryKind::Directory, 0, 0),
            ("Linked/inner.txt", EntryKind::File, 1, 1),
            ("LINKED", EntryKind::Directory, 0, 0),
            ("dir", EntryKind::Directory, 0, 0),
            ("dir/system.dat", EntryKind::File, 1, 1),
            ("dir/SYSTEM.DAT", EntryKind::File, 1, 1),
            ("gone", EntryKind::Directory, 0, 0),
            ("gone/system.dat", EntryKind::File, 1, 1),
            ("stale.txt", EntryKind::File, 1, 1),
        ]);
        let plan = build_plan(
            &root,
            &local_inventory,
            &remote_inventory,
            &rules(&[]),
            &options(true),
        )
        .unwrap();
        assert_eq!(deleted(&plan), ["gone/system.dat", "gone", "stale.txt"]);
        assert_eq!(
            skips(&plan),
            [
                ("Linked", "link-or-reparse-point"),
                ("dir/system.dat", "windows-attributes"),
            ]
        );
    }

    /// N-5: an NFD local name and its NFC remote spelling are one entry. Nothing is uploaded beside
    /// the remote spelling, nothing is deleted, and a change is written to the spelling the
    /// destination already holds -- including for a new file inside a paired directory.
    #[test]
    fn unicode_normalisation_variants_pair_instead_of_upload_and_delete() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let composed = "caf\u{e9}";
        let decomposed = "cafe\u{301}";
        for (local_name, remote_name) in [(decomposed, composed), (composed, decomposed)] {
            let file = |name: &str| format!("{name}.txt");
            let inner = |name: &str, child: &str| format!("{name}/{child}");
            let local_inventory = local(&[
                (&file(local_name), EntryKind::File, 4, 4_000),
                (local_name, EntryKind::Directory, 0, 0),
                (&inner(local_name, "same.txt"), EntryKind::File, 2, 2_000),
                (&inner(local_name, "new.txt"), EntryKind::File, 3, 3_000),
            ]);
            let remote_inventory = remote(&[
                (&file(remote_name), EntryKind::File, 4, 1),
                (remote_name, EntryKind::Directory, 0, 0),
                (&inner(remote_name, "same.txt"), EntryKind::File, 2, 2),
                (&inner(remote_name, "stale.txt"), EntryKind::File, 1, 1),
            ]);
            let plan = build_plan(
                &root,
                &local_inventory,
                &remote_inventory,
                &rules(&[]),
                &options(true),
            )
            .unwrap();
            assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
            assert!(
                plan.creates.is_empty(),
                "the directory already exists remotely"
            );
            assert_eq!(plan.unchanged_files, 1);
            let new_inner = inner(local_name, "new.txt");
            let changed = file(local_name);
            let new_target = format!("/share/root/{}", inner(remote_name, "new.txt"));
            let changed_target = format!("/share/root/{}", file(remote_name));
            let mut expected = vec![
                (new_inner.as_str(), new_target.as_str()),
                (changed.as_str(), changed_target.as_str()),
            ];
            expected.sort();
            assert_eq!(uploaded(&plan), expected);
            assert_eq!(
                deleted(&plan),
                [inner(remote_name, "stale.txt").as_str()],
                "only the genuinely remote-only entry is deleted"
            );

            // The status listing pairs the same way: one row per name, not missing + remote-only.
            let page = build_status_page(
                &root,
                &local_inventory,
                &remote_inventory,
                &BTreeSet::new(),
                &rules(&[]),
                &StatusQuery {
                    scope: &Scope::root(),
                    compare: CompareMode::Metadata,
                    filter: None,
                    states: StateFilter::All,
                    include_excluded: false,
                    limit: 50,
                    cursor: None,
                    local_complete: true,
                    remote_complete: true,
                },
            )
            .unwrap();
            assert_eq!(page.stats.remote_only_entries, 1);
            assert_eq!(page.stats.missing_remote_files, 1);
            assert_eq!(page.stats.in_sync_files, 1);
        }
    }

    /// The digest selections pair the same way the planner does, so a paired file gets the remote
    /// digest its comparison needs and is not taken for a deletion candidate.
    #[test]
    fn digest_selection_follows_unicode_pairing() {
        let local_inventory = local(&[("cafe\u{301}.txt", EntryKind::File, 4, 4_000)]);
        let remote_inventory = remote(&[("caf\u{e9}.txt", EntryKind::File, 4, 4)]);
        assert_eq!(
            select_comparison_remote_digests(&local_inventory, &remote_inventory, &rules(&[])),
            BTreeSet::from(["caf\u{e9}.txt".to_owned()])
        );
        assert!(
            select_deletion_guard_hashes(&local_inventory, &remote_inventory, &rules(&[]))
                .is_empty()
        );
    }

    /// N-7: two empty files are equal without a digest, so content mode neither asks DSM to hash
    /// them nor uploads them every run, and an empty remote-only file is deleted on its size guard.
    #[test]
    fn empty_files_need_no_digest_to_compare_or_to_delete() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let local_inventory = local(&[
            ("empty.txt", EntryKind::File, 0, 1_000),
            ("keep.txt", EntryKind::File, 1, 1_000),
        ]);
        let mut remote_inventory = remote(&[
            ("empty.txt", EntryKind::File, 0, 1),
            ("keep.txt", EntryKind::File, 1, 1),
            ("gone.txt", EntryKind::File, 0, 1),
        ]);
        remote_inventory
            .entries
            .get_mut("keep.txt")
            .unwrap()
            .content_md5 = Some(digest(1));
        let mut local_inventory = local_inventory;
        local_inventory
            .entries
            .get_mut("keep.txt")
            .unwrap()
            .content_md5 = Some(digest(1));

        assert_eq!(
            select_comparison_remote_digests(&local_inventory, &remote_inventory, &rules(&[])),
            BTreeSet::from(["keep.txt".to_owned()])
        );
        assert!(
            select_deletion_guard_hashes(&local_inventory, &remote_inventory, &rules(&[]))
                .is_empty()
        );
        let plan = build_plan(
            &root,
            &local_inventory,
            &remote_inventory,
            &rules(&[]),
            &content_options(true, false),
        )
        .unwrap();
        assert!(plan.uploads.is_empty());
        assert_eq!(plan.unchanged_files, 2);
        assert_eq!(deleted(&plan), ["gone.txt"]);
        assert_eq!(plan.post_deletes[0].snapshot.size, 0);
        assert_eq!(plan.post_deletes[0].snapshot.content_md5, None);
    }

    /// A file withheld after the walk (it changed while being hashed) is shielded on every
    /// spelling: its NFC remote partner is neither deleted nor overwritten, and it is reported.
    #[test]
    fn a_withheld_path_is_never_deleted_on_any_spelling() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let mut local_inventory = local(&[
            ("cafe\u{301}.txt", EntryKind::File, 4, 4_000),
            ("keep.txt", EntryKind::File, 1, 1_000),
        ]);
        local_inventory.withhold(
            "cafe\u{301}.txt",
            SkipReason::Deferred {
                detail: "changed while hashing".to_owned(),
            },
        );
        let plan = build_plan(
            &root,
            &local_inventory,
            &remote(&[
                ("caf\u{e9}.txt", EntryKind::File, 9, 9),
                ("keep.txt", EntryKind::File, 1, 1),
            ]),
            &rules(&[]),
            &options(true),
        )
        .unwrap();
        assert!(deleted(&plan).is_empty());
        assert!(plan.uploads.is_empty());
        assert_eq!(skips(&plan), [("cafe\u{301}.txt", "deferred")]);
    }

    /// An overwrite carries the remote state it was planned against; a new file carries none.
    #[test]
    fn an_overwrite_records_the_remote_file_it_replaces() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let plan = build_plan(
            &root,
            &local(&[
                ("changed.txt", EntryKind::File, 4, 5_000),
                ("new.txt", EntryKind::File, 3, 2_000),
            ]),
            &remote(&[("changed.txt", EntryKind::File, 7, 3)]),
            &rules(&[]),
            &options(false),
        )
        .unwrap();
        let expected: Vec<_> = plan
            .uploads
            .iter()
            .map(|upload| {
                (
                    upload.local.relative.as_str(),
                    upload
                        .expected_remote
                        .as_ref()
                        .map(|snapshot| (snapshot.kind, snapshot.size, snapshot.mtime_seconds)),
                )
            })
            .collect();
        assert_eq!(
            expected,
            [
                ("changed.txt", Some((EntryKind::File, 7, 3))),
                ("new.txt", None),
            ]
        );
    }

    #[test]
    fn mtimes_agree_under_floor_or_round_and_nothing_wider() {
        // A DSM that floors: 3.7 s is stored as 3.
        assert!(mtimes_agree(3_700, 3));
        // A DSM that rounds: 3.7 s is stored as 4, and 3.5 s rounds half-up to 4.
        assert!(mtimes_agree(3_700, 4));
        assert!(mtimes_agree(3_500, 4));
        // Below .5 both conventions give the same second, so the next one is a change.
        assert!(mtimes_agree(3_200, 3));
        assert!(!mtimes_agree(3_200, 4));
        // 1.2 s apart is a change even though both readings are fine-grained locally.
        assert!(!mtimes_agree(4_200, 3));
        assert!(!mtimes_agree(1_800, 3));
        // Whole second against whole second: exact, so one second apart is a change.
        assert!(mtimes_agree(3_000, 3));
        assert!(!mtimes_agree(3_000, 2));
        assert!(!mtimes_agree(3_000, 4));
        // Before the epoch, floor and round still mean what they mean.
        assert!(mtimes_agree(-1, -1));
        assert!(mtimes_agree(-1, 0));
        assert!(mtimes_agree(-1_500, -2));
        assert!(mtimes_agree(-1_500, -1));
        assert!(!mtimes_agree(-1_000, 0));
        assert!(!mtimes_agree(i64::MIN, i64::MAX));
        assert!(!mtimes_agree(i64::MAX, i64::MIN));
    }

    /// N-8: the descendant test is a range query. Siblings that sort between a directory and its
    /// children (`a-b` sorts between `a` and `a/`) must not be mistaken for descendants.
    #[test]
    fn descendant_queries_respect_component_boundaries() {
        let inventory = remote(&[
            ("a", EntryKind::Directory, 0, 0),
            ("a-b", EntryKind::Directory, 0, 0),
            ("a-b/x", EntryKind::File, 1, 1),
            ("a/x", EntryKind::File, 1, 1),
            ("b", EntryKind::Directory, 0, 0),
            ("b-c", EntryKind::File, 1, 1),
        ]);
        let snapshot = |relative: &str| {
            deletion_snapshot(
                &inventory.entries[relative],
                &inventory.entries,
                CompareMode::Metadata,
            )
            .unwrap()
        };
        assert!(!snapshot("a").require_mtime, "a has a descendant");
        assert!(!snapshot("a-b").require_mtime);
        assert!(snapshot("b").require_mtime, "b-c is a sibling, not a child");
        assert_eq!(
            remote_subtree(&inventory.entries, "a")
                .iter()
                .map(|entry| entry.relative.as_str())
                .collect::<Vec<_>>(),
            ["a", "a/x"]
        );
        assert_eq!(
            remote_subtree(&inventory.entries, "b")
                .iter()
                .map(|entry| entry.relative.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
    }

    /// A case collision the plan will never touch must not fail the run.
    ///
    /// A photo share keeps one `@eaDir` thumbnail directory per image, so a volume that has held
    /// both `Photo.JPG` and `photo.jpg` has two of them; a recycle bin accumulates `Report.pdf` and
    /// `report.pdf` deleted on different days. Every `sync`, `plan`, `status` and `resync` then
    /// failed with `UnsafeRemotePath` naming a path the tool documents as never synchronized, and
    /// `--exclude '#recycle/'` did not help because the check ran before and independently of the
    /// rules. The operator's only remedy was to empty the NAS recycle bin or delete thumbnails.
    #[test]
    fn a_case_collision_among_paths_the_plan_never_touches_is_not_refused() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        for collision in [
            &[
                ("@eaDir", EntryKind::Directory, 0, 0),
                ("@eaDir/Photo.JPG", EntryKind::Directory, 0, 0),
                ("@eaDir/photo.jpg", EntryKind::Directory, 0, 0),
            ][..],
            &[
                ("#recycle", EntryKind::Directory, 0, 0),
                ("#recycle/Report.pdf", EntryKind::File, 1, 1),
                ("#recycle/report.pdf", EntryKind::File, 1, 1),
            ][..],
            // Excluded by an ordinary `--exclude`, which must work here for the same reason.
            &[
                ("cache", EntryKind::Directory, 0, 0),
                ("cache/Data.bin", EntryKind::File, 1, 1),
                ("cache/data.bin", EntryKind::File, 1, 1),
            ][..],
        ] {
            let plan = build_plan(
                &root,
                &local(&[("keep.txt", EntryKind::File, 1, 1_000)]),
                &remote(collision),
                &rules(&["cache/"]),
                &options(false),
            )
            .unwrap_or_else(|error| panic!("{collision:?}: {error}"));
            assert_eq!(plan.uploads.len(), 1);
            assert!(plan.skipped.is_empty(), "{collision:?}");
        }

        // A collision between two paths the plan *would* touch is reported, and left alone.
        let plan = build_plan(
            &root,
            &local(&[]),
            &remote(&[
                ("Payload", EntryKind::Directory, 0, 0),
                ("payload", EntryKind::Directory, 0, 0),
            ]),
            &rules(&["cache/"]),
            &options(false),
        )
        .unwrap();
        assert_eq!(
            skips(&plan),
            [
                ("Payload", "remote-case-collision"),
                ("payload", "remote-case-collision"),
            ]
        );
    }

    #[test]
    fn delete_mode_preserves_excluded_and_managed_paths() {
        let local = local(&[("keep.txt", EntryKind::File, 1, 1_000)]);
        let remote = remote(&[
            ("keep.txt", EntryKind::File, 1, 1),
            ("remove.txt", EntryKind::File, 1, 1),
            ("cache", EntryKind::Directory, 0, 0),
            ("cache/data.bin", EntryKind::File, 1, 1),
            ("@eaDir", EntryKind::Directory, 0, 0),
        ]);
        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&["cache/data.bin"]),
            &options(true),
        )
        .unwrap();
        assert_eq!(
            plan.post_deletes
                .iter()
                .map(|action| action.relative.as_str())
                .collect::<Vec<_>>(),
            ["remove.txt"]
        );
    }

    #[test]
    fn type_conflict_requires_delete_and_is_ordered_deepest_first() {
        let local = local(&[("node", EntryKind::File, 1, 1_000)]);
        let remote = remote(&[
            ("node", EntryKind::Directory, 0, 0),
            ("node/child", EntryKind::Directory, 0, 0),
            ("node/child/file", EntryKind::File, 1, 1),
        ]);
        let root = RemoteRoot::parse("/share/root").unwrap();
        assert!(build_plan(&root, &local, &remote, &rules(&[]), &options(false)).is_err());
        let plan = build_plan(&root, &local, &remote, &rules(&[]), &options(true)).unwrap();
        assert_eq!(
            plan.pre_deletes
                .iter()
                .map(|action| action.relative.as_str())
                .collect::<Vec<_>>(),
            ["node/child/file", "node/child", "node"]
        );
        assert_eq!(plan.uploads.len(), 1);
    }

    #[test]
    fn refuses_unbounded_or_empty_source_deletion() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let empty_local = local(&[]);
        let remote = remote(&[("only.txt", EntryKind::File, 1, 1)]);
        assert!(matches!(
            build_plan(&root, &empty_local, &remote, &rules(&[]), &options(true)),
            Err(Error::EmptySourceDeletion)
        ));

        let directory_only = local(&[("keep", EntryKind::Directory, 0, 0)]);
        assert!(matches!(
            build_plan(&root, &directory_only, &remote, &rules(&[]), &options(true)),
            Err(Error::EmptySourceDeletion)
        ));

        let local_inventory = local(&[("keep.txt", EntryKind::File, 1, 1_000)]);
        let mut strict = options(true);
        strict.max_delete = 0;
        assert!(matches!(
            build_plan(&root, &local_inventory, &remote, &rules(&[]), &strict),
            Err(Error::DeleteLimit { .. })
        ));
    }

    #[test]
    fn protects_remote_mount_boundaries() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let local_inventory = local(&[("keep.txt", EntryKind::File, 1, 1_000)]);
        let mut remote = remote(&[
            ("keep.txt", EntryKind::File, 1, 1),
            ("archive", EntryKind::Directory, 0, 0),
        ]);
        remote.entries.get_mut("archive").unwrap().mount_point_type = Some("cifs".to_owned());

        let plan = build_plan(
            &root,
            &local_inventory,
            &remote,
            &rules(&[]),
            &options(true),
        )
        .unwrap();
        assert!(plan.post_deletes.is_empty());
        assert_eq!(plan.protected_entries, 1);

        let local_below_mount = local(&[
            ("keep.txt", EntryKind::File, 1, 1_000),
            ("archive/new.txt", EntryKind::File, 1, 1_000),
        ]);
        assert!(matches!(
            build_plan(
                &root,
                &local_below_mount,
                &remote,
                &rules(&[]),
                &options(false)
            ),
            Err(Error::ProtectedConflict(_))
        ));
    }

    #[test]
    fn content_mode_detects_same_size_same_mtime_changes() {
        let mut local = local(&[("same-metadata.bin", EntryKind::File, 4, 3_000)]);
        let mut remote = remote(&[("same-metadata.bin", EntryKind::File, 4, 3)]);
        local
            .entries
            .get_mut("same-metadata.bin")
            .unwrap()
            .content_md5 = Some(digest(1));
        remote
            .entries
            .get_mut("same-metadata.bin")
            .unwrap()
            .content_md5 = Some(digest(2));

        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, false),
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert_eq!(plan.unchanged_files, 0);
        assert_eq!(plan.uploads[0].reason, ChangeReason::ContentDiffers);
    }

    /// 3.499 s floors and rounds to 3, so a remote 4 is a change. (3.5 s or later would round to 4
    /// and agree: DSM may have stored it that way.)
    #[test]
    fn content_mode_detects_mtime_only_changes_at_file_station_resolution() {
        let mut local = local(&[("mtime-only.bin", EntryKind::File, 4, 3_499)]);
        let mut remote = remote(&[("mtime-only.bin", EntryKind::File, 4, 4)]);
        local.entries.get_mut("mtime-only.bin").unwrap().content_md5 = Some(digest(6));
        remote
            .entries
            .get_mut("mtime-only.bin")
            .unwrap()
            .content_md5 = Some(digest(6));

        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, false),
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert_eq!(plan.unchanged_files, 0);
        assert_eq!(plan.uploads[0].reason, ChangeReason::MtimeDiffers);
    }

    #[test]
    fn unique_cross_parent_content_match_becomes_non_destructive_server_copy() {
        let mut local = local(&[("new/report.bin", EntryKind::File, 4, 3_000)]);
        let mut remote = remote(&[("old/report.bin", EntryKind::File, 4, 3)]);
        local.entries.get_mut("new/report.bin").unwrap().content_md5 = Some(digest(7));
        remote
            .entries
            .get_mut("old/report.bin")
            .unwrap()
            .content_md5 = Some(digest(7));

        let root = RemoteRoot::parse("/share/root").unwrap();
        let additive = build_plan(
            &root,
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, true),
        )
        .unwrap();
        assert!(additive.uploads.is_empty());
        assert_eq!(additive.copies.len(), 1);
        assert!(additive.post_deletes.is_empty());
        assert_eq!(additive.upload_bytes, 0);

        let mirror = build_plan(
            &root,
            &local,
            &remote,
            &rules(&[]),
            &content_options(true, true),
        )
        .unwrap();
        assert_eq!(mirror.copies.len(), 1);
        assert_eq!(mirror.post_deletes.len(), 1);
        assert_eq!(mirror.post_deletes[0].relative, "old/report.bin");
        let guard = mirror.post_deletes[0].destination_guard.as_ref().unwrap();
        assert_eq!(guard.remote_path, "/share/root/new/report.bin");
        assert_eq!(guard.expected_size, 4);
        assert_eq!(guard.expected_mtime_seconds, 3);
        assert_eq!(guard.local.relative, "new/report.bin");
    }

    #[test]
    fn server_copy_candidate_with_wrong_mtime_keeps_upload_fallback() {
        let mut local = local(&[("new/report.bin", EntryKind::File, 4, 3_000)]);
        let mut remote = remote(&[("old/report.bin", EntryKind::File, 4, 2)]);
        local.entries.get_mut("new/report.bin").unwrap().content_md5 = Some(digest(7));
        remote
            .entries
            .get_mut("old/report.bin")
            .unwrap()
            .content_md5 = Some(digest(7));

        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, true),
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert!(plan.copies.is_empty());
    }

    #[test]
    fn basename_change_keeps_verified_upload_fallback() {
        let mut local = local(&[("folder/new.bin", EntryKind::File, 4, 3_000)]);
        let mut remote = remote(&[("folder/old.bin", EntryKind::File, 4, 3)]);
        local.entries.get_mut("folder/new.bin").unwrap().content_md5 = Some(digest(4));
        remote
            .entries
            .get_mut("folder/old.bin")
            .unwrap()
            .content_md5 = Some(digest(4));

        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, true),
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert!(plan.copies.is_empty());
    }

    #[test]
    fn duplicate_content_matches_keep_verified_upload_fallback() {
        let mut local = local(&[("new/report.bin", EntryKind::File, 4, 3_000)]);
        let mut remote = remote(&[
            ("old-a/report.bin", EntryKind::File, 4, 3),
            ("old-b/report.bin", EntryKind::File, 4, 3),
        ]);
        local.entries.get_mut("new/report.bin").unwrap().content_md5 = Some(digest(9));
        for entry in remote.entries.values_mut() {
            entry.content_md5 = Some(digest(9));
        }

        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, true),
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert!(plan.copies.is_empty());
    }

    #[test]
    fn same_digest_with_different_size_is_never_reused() {
        let mut local = local(&[("new/report.bin", EntryKind::File, 5, 3_000)]);
        let mut remote = remote(&[("old/report.bin", EntryKind::File, 4, 3)]);
        local.entries.get_mut("new/report.bin").unwrap().content_md5 = Some(digest(3));
        remote
            .entries
            .get_mut("old/report.bin")
            .unwrap()
            .content_md5 = Some(digest(3));

        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, true),
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert!(plan.copies.is_empty());
    }

    #[test]
    fn unavailable_copy_api_does_not_require_remote_only_hashes() {
        let mut local = local(&[("new/report.bin", EntryKind::File, 4, 3_000)]);
        let remote = remote(&[("old/report.bin", EntryKind::File, 4, 1)]);
        local.entries.get_mut("new/report.bin").unwrap().content_md5 = Some(digest(4));

        let plan = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &content_options(false, false),
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert!(plan.copies.is_empty());
    }

    #[test]
    fn remote_hash_selection_is_minimal_and_protection_aware() {
        let local = local(&[
            ("same.bin", EntryKind::File, 4, 1_000),
            ("new/report.bin", EntryKind::File, 4, 1_000),
            ("ignored.bin", EntryKind::File, 4, 1_000),
        ]);
        let remote = remote(&[
            ("same.bin", EntryKind::File, 4, 1),
            ("old/report.bin", EntryKind::File, 4, 1),
            ("ignored.bin", EntryKind::File, 4, 1),
            ("other/report.bin", EntryKind::File, 9, 1),
            ("@eaDir/report.bin", EntryKind::File, 4, 1),
        ]);
        let ignore = rules(&["ignored.bin"]);

        assert_eq!(
            select_remote_content_hashes(&local, &remote, &ignore, false),
            BTreeSet::from(["same.bin".to_owned()])
        );
        assert_eq!(
            select_remote_content_hashes(&local, &remote, &ignore, true),
            BTreeSet::from(["old/report.bin".to_owned(), "same.bin".to_owned()])
        );
        assert_eq!(
            select_remote_content_hashes_for_plan(&local, &remote, &ignore, false, true),
            BTreeSet::from([
                "old/report.bin".to_owned(),
                "other/report.bin".to_owned(),
                "same.bin".to_owned()
            ])
        );
    }

    #[test]
    fn missing_share_root_is_not_created_but_a_missing_subdirectory_is() {
        let local = local(&[("payload.bin", EntryKind::File, 4, 1_000)]);
        let mut missing_remote = remote(&[]);
        missing_remote.root_exists = false;

        assert!(matches!(
            build_plan(
                &RemoteRoot::parse("/share").unwrap(),
                &local,
                &missing_remote,
                &rules(&[]),
                &options(false),
            ),
            Err(Error::ShareNotWritable(share)) if share == "share"
        ));

        let plan = build_plan(
            &RemoteRoot::parse("/share/new-root").unwrap(),
            &local,
            &missing_remote,
            &rules(&[]),
            &options(false),
        )
        .unwrap();
        assert_eq!(
            plan.creates
                .iter()
                .map(|action| (action.relative.as_str(), action.remote_path.as_str()))
                .collect::<Vec<_>>(),
            [("", "/share/new-root")]
        );
        assert_eq!(plan.creates[0].reason, ChangeReason::MissingRemote);
        assert_eq!(plan.uploads[0].remote_path, "/share/new-root/payload.bin");
    }

    #[test]
    fn content_mode_mirror_delete_requires_a_plan_time_remote_digest() {
        let mut local = local(&[("keep.bin", EntryKind::File, 4, 1_000)]);
        local.entries.get_mut("keep.bin").unwrap().content_md5 = Some(digest(1));
        let mut remote = remote(&[
            ("extra.bin", EntryKind::File, 4, 1),
            ("keep.bin", EntryKind::File, 4, 1),
        ]);
        remote.entries.get_mut("keep.bin").unwrap().content_md5 = Some(digest(1));

        let error = build_plan(
            &RemoteRoot::parse("/share/root").unwrap(),
            &local,
            &remote,
            &rules(&[]),
            &content_options(true, false),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            Error::Message(message)
                if message.contains("plan-time MD5/CRC32/SHA-256 fingerprint")
                    && message.contains("/share/root/extra.bin")
        ));
    }

    #[test]
    fn server_copy_planning_fails_closed_without_a_local_digest() {
        let local = local(&[("new/report.bin", EntryKind::File, 4, 1_000)]);

        assert!(matches!(
            build_plan(
                &RemoteRoot::parse("/share/root").unwrap(),
                &local,
                &remote(&[]),
                &rules(&[]),
                &content_options(false, true),
            ),
            Err(Error::Message(message))
                if message == "content comparison requires every local file digest"
        ));
    }
}

#[cfg(test)]
mod status_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn root() -> RemoteRoot {
        RemoteRoot::parse("/share/root").unwrap()
    }

    fn local(entries: &[(&str, EntryKind, u64, i64)]) -> LocalInventory {
        LocalInventory {
            root: PathBuf::from("/source"),
            entries: entries
                .iter()
                .map(|(relative, kind, size, mtime_ms)| {
                    (
                        (*relative).to_owned(),
                        LocalEntry {
                            relative: (*relative).to_owned(),
                            full_path: PathBuf::from(relative),
                            kind: *kind,
                            size: *size,
                            mtime_ms: *mtime_ms,
                            identity: Default::default(),
                            content_md5: None,
                        },
                    )
                })
                .collect(),
            skipped: Vec::new(),
        }
    }

    fn remote(entries: &[(&str, EntryKind, u64, i64)]) -> RemoteInventory {
        RemoteInventory {
            root_exists: true,
            entries: entries
                .iter()
                .map(|(relative, kind, size, mtime_seconds)| {
                    (
                        (*relative).to_owned(),
                        RemoteEntry {
                            relative: (*relative).to_owned(),
                            remote_path: format!("/share/root/{relative}"),
                            kind: *kind,
                            size: *size,
                            mtime_seconds: *mtime_seconds,
                            mount_point_type: None,
                            content_md5: None,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn rules(patterns: &[&str]) -> IgnoreRules {
        let root = std::env::temp_dir().join(format!("sdsync-status-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        IgnoreRules::build(
            &root,
            &patterns
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn query(scope: &Scope) -> StatusQuery<'_> {
        StatusQuery {
            local_complete: true,
            remote_complete: true,
            scope,
            compare: CompareMode::Metadata,
            filter: None,
            states: StateFilter::All,
            include_excluded: false,
            limit: STATUS_PAGE_SIZE_DEFAULT,
            cursor: None,
        }
    }

    fn page(
        local: &LocalInventory,
        remote: &RemoteInventory,
        query: &StatusQuery<'_>,
    ) -> StatusPage {
        build_status_page(&root(), local, remote, &BTreeSet::new(), &rules(&[]), query).unwrap()
    }

    fn states(page: &StatusPage) -> Vec<(&str, StateKind)> {
        page.entries
            .iter()
            .map(|entry| (entry.relative.as_str(), entry.state.kind()))
            .collect()
    }

    fn names(page: &StatusPage) -> Vec<&str> {
        page.entries
            .iter()
            .map(|entry| entry.relative.as_str())
            .collect()
    }

    /// A truncated local walk must not turn in-sync files into `remote-only` rows.
    ///
    /// Six identical in-sync pairs with the local side truncated to three produced three
    /// `remote-only` rows and counted all three as needing attention — and `RemoteOnly` documents
    /// itself as "present remotely, absent locally; removed only by a mirror run", so the dashboard
    /// told the operator that a mirror run would delete files that were present locally and
    /// identical. Truncation makes `in_sync_files` a floor and `remote_only_entries` an *over*count,
    /// which is why one notice claiming every total was a lower bound was false for exactly the
    /// totals truncation inflates.
    #[test]
    fn a_truncated_local_walk_produces_no_remote_only_rows() {
        // The local side records milliseconds and the remote side whole seconds, so an in-sync
        // pair is 1_000 against 1.
        let locals = [
            ("a.txt", EntryKind::File, 1_u64, 1_000_i64),
            ("b.txt", EntryKind::File, 1, 1_000),
            ("c.txt", EntryKind::File, 1, 1_000),
        ];
        let remotes = [
            ("a.txt", EntryKind::File, 1_u64, 1_i64),
            ("b.txt", EntryKind::File, 1, 1),
            ("c.txt", EntryKind::File, 1, 1),
        ];
        // The local walk stopped after the first entry; the remote walk saw all three.
        let truncated_local = local(&locals[..1]);
        let whole_remote = remote(&remotes);
        let scope = Scope::root();

        let page = page(
            &truncated_local,
            &whole_remote,
            &StatusQuery {
                local_complete: false,
                ..query(&scope)
            },
        );

        assert!(
            !states(&page)
                .iter()
                .any(|(_, kind)| *kind == StateKind::RemoteOnly),
            "presence alone cannot decide remote-only when the local walk was truncated: {:?}",
            states(&page)
        );
        assert_eq!(page.stats.remote_only_entries, 0);
        assert_eq!(page.stats.not_compared_entries, 2);
        assert_eq!(page.stats.in_sync_files, 1);
        // The two unclassifiable entries are not counted as needing attention, which is the number
        // the dashboard renders with a "+" after it.
        assert_eq!(page.stats.attention_entries, 0);
    }

    /// The mirror image: a truncated remote walk must not turn in-sync files into `missing-remote`.
    #[test]
    fn a_truncated_remote_walk_produces_no_missing_remote_rows() {
        let whole_local = local(&[
            ("a.txt", EntryKind::File, 1, 1_000),
            ("b.txt", EntryKind::File, 1, 1_000),
        ]);
        let truncated_remote = remote(&[("a.txt", EntryKind::File, 1, 1)]);
        let scope = Scope::root();

        let page = page(
            &whole_local,
            &truncated_remote,
            &StatusQuery {
                remote_complete: false,
                ..query(&scope)
            },
        );

        assert_eq!(page.stats.missing_remote_files, 0);
        assert_eq!(page.stats.not_compared_entries, 1);
        assert_eq!(page.stats.attention_entries, 0);
    }

    /// With both walks complete, absence is evidence again and nothing changes.
    #[test]
    fn a_complete_pair_of_walks_still_reports_remote_only_and_missing_remote() {
        let scope = Scope::root();
        let page = page(
            &local(&[("only-local.txt", EntryKind::File, 1, 1)]),
            &remote(&[("only-remote.txt", EntryKind::File, 1, 1)]),
            &query(&scope),
        );

        assert_eq!(page.stats.not_compared_entries, 0);
        assert_eq!(page.stats.missing_remote_files, 1);
        assert_eq!(page.stats.remote_only_entries, 1);
        assert_eq!(page.stats.attention_entries, 2);
    }

    // --- Scope ------------------------------------------------------------

    #[test]
    fn scope_parse_normalizes_surrounding_slashes_and_accepts_the_empty_root() {
        assert!(Scope::parse("").unwrap().is_root());
        assert!(Scope::parse("/").unwrap().is_root());
        assert_eq!(Scope::parse("docs").unwrap().as_str(), "docs");
        assert_eq!(Scope::parse("/docs/").unwrap().as_str(), "docs");
        assert_eq!(
            Scope::parse("docs/q3/a.txt").unwrap().as_str(),
            "docs/q3/a.txt"
        );
    }

    #[test]
    fn scope_parse_rejects_traversal_backslashes_and_dsm_managed_names() {
        for value in [
            "../escape",
            "docs/../..",
            "docs/./here",
            "a\\b",
            "@eaDir",
            "#recycle/x",
        ] {
            assert!(
                Scope::parse(value).is_err(),
                "scope {value:?} should have been rejected"
            );
        }
    }

    #[test]
    fn scope_matches_only_on_component_boundaries() {
        let scope = Scope::parse("docs").unwrap();
        assert!(scope.matches("docs"));
        assert!(scope.matches("docs/a.txt"));
        assert!(scope.matches("docs/deep/b.txt"));
        // Component-wise: a sibling that merely starts with the same letters is out of scope.
        assert!(!scope.matches("docsets"));
        assert!(!scope.matches("docsets/a.txt"));
        assert!(!scope.matches("other"));
    }

    #[test]
    fn a_file_scope_matches_itself_and_nothing_else() {
        let scope = Scope::parse("docs/a.txt").unwrap();
        assert!(scope.matches("docs/a.txt"));
        assert!(!scope.matches("docs"));
        assert!(!scope.matches("docs/b.txt"));
    }

    #[test]
    fn the_root_scope_matches_everything() {
        let scope = Scope::root();
        assert!(scope.matches(""));
        assert!(scope.matches("anything/at/all"));
        assert!(!scope.is_ancestor_of_scope("anything"));
    }

    #[test]
    fn ancestors_of_a_scope_are_recognized_but_are_not_in_scope() {
        let scope = Scope::parse("a/b/c.txt").unwrap();
        for ancestor in ["", "a", "a/b"] {
            assert!(
                scope.is_ancestor_of_scope(ancestor),
                "{ancestor:?} should be an ancestor of the scope"
            );
            assert!(
                !scope.matches(ancestor),
                "{ancestor:?} is an ancestor and must not be in scope"
            );
        }
        assert!(!scope.is_ancestor_of_scope("a/b/c.txt"));
        assert!(!scope.is_ancestor_of_scope("a/z"));
    }

    // --- Force ------------------------------------------------------------

    #[test]
    fn force_reports_forced_without_consulting_size_or_time() {
        let identical_local = LocalEntry {
            relative: "a.txt".to_owned(),
            full_path: PathBuf::from("a.txt"),
            kind: EntryKind::File,
            size: 10,
            mtime_ms: 5_000,
            identity: Default::default(),
            content_md5: None,
        };
        let identical_remote = RemoteEntry {
            relative: "a.txt".to_owned(),
            remote_path: "/share/root/a.txt".to_owned(),
            kind: EntryKind::File,
            size: 10,
            mtime_seconds: 5,
            mount_point_type: None,
            content_md5: None,
        };
        // Byte-for-byte identical under every other mode.
        assert_eq!(
            compare_files(&identical_local, &identical_remote, CompareMode::Metadata),
            None
        );
        assert_eq!(
            compare_files(&identical_local, &identical_remote, CompareMode::Force),
            Some(ChangeReason::Forced)
        );

        // A differing pair still reports Forced, never a difference force never looked for.
        let bigger = LocalEntry {
            size: 99,
            ..identical_local.clone()
        };
        assert_eq!(
            compare_files(&bigger, &identical_remote, CompareMode::Force),
            Some(ChangeReason::Forced)
        );
    }

    #[test]
    fn force_keeps_content_grade_deletion_guards() {
        // The guard must not weaken as a side effect of a flag that was about uploading.
        assert!(CompareMode::Content.requires_deletion_digest());
        assert!(CompareMode::Force.requires_deletion_digest());
        assert!(!CompareMode::Metadata.requires_deletion_digest());
        assert!(!CompareMode::SizeOnly.requires_deletion_digest());
    }

    #[test]
    fn a_forced_mirror_refuses_to_delete_without_a_plan_time_digest() {
        let local = local(&[("keep.txt", EntryKind::File, 1, 1_000)]);
        let remote = remote(&[
            ("keep.txt", EntryKind::File, 1, 1),
            ("gone.txt", EntryKind::File, 4, 2),
        ]);
        let error = build_plan(
            &root(),
            &local,
            &remote,
            &rules(&[]),
            &PlanOptions {
                delete: true,
                allow_empty_source: false,
                max_delete: 100,
                compare: CompareMode::Force,
                server_copy: false,
                scope: Scope::root(),
            },
        )
        .unwrap_err();
        assert!(
            matches!(&error, Error::Message(message) if message.contains("fingerprint")),
            "forced mirror deletion must fail closed without a digest, got {error:?}"
        );
    }

    #[test]
    fn the_deletion_guard_selection_covers_only_delete_candidates() {
        let local = local(&[("keep.txt", EntryKind::File, 1, 1_000)]);
        let remote = remote(&[
            ("keep.txt", EntryKind::File, 1, 1),
            ("gone.txt", EntryKind::File, 4, 2),
            ("also-gone.txt", EntryKind::File, 4, 2),
        ]);
        let selected = select_deletion_guard_hashes(&local, &remote, &rules(&[]));
        assert_eq!(
            selected.iter().map(String::as_str).collect::<Vec<_>>(),
            ["also-gone.txt", "gone.txt"]
        );
    }

    #[test]
    fn forced_uploads_carry_a_reason_that_names_the_overwrite() {
        assert_eq!(ChangeReason::Forced.as_str(), "forced");
        assert!(ChangeReason::Forced.detail().contains("overwrites"));

        let local = local(&[("a.txt", EntryKind::File, 10, 5_000)]);
        let remote = remote(&[("a.txt", EntryKind::File, 10, 5)]);
        let plan = build_plan(
            &root(),
            &local,
            &remote,
            &rules(&[]),
            &PlanOptions {
                delete: false,
                allow_empty_source: false,
                max_delete: 100,
                compare: CompareMode::Force,
                server_copy: false,
                scope: Scope::root(),
            },
        )
        .unwrap();
        assert_eq!(plan.uploads.len(), 1);
        assert_eq!(plan.uploads[0].reason, ChangeReason::Forced);
        assert_eq!(plan.unchanged_files, 0);
        assert_eq!(plan.upload_bytes, 10);
    }

    // --- Per-entry classification ----------------------------------------

    #[test]
    fn every_status_state_is_reported_from_the_same_comparison_the_planner_uses() {
        let local = local(&[
            ("dir", EntryKind::Directory, 0, 0),
            ("same.txt", EntryKind::File, 4, 3_500),
            ("changed.txt", EntryKind::File, 4, 5_000),
            ("new.txt", EntryKind::File, 3, 2_000),
            ("conflict", EntryKind::Directory, 0, 0),
        ]);
        let remote = remote(&[
            ("dir", EntryKind::Directory, 0, 0),
            ("same.txt", EntryKind::File, 4, 3),
            ("changed.txt", EntryKind::File, 4, 4),
            ("extra.txt", EntryKind::File, 1, 1),
            ("conflict", EntryKind::File, 7, 9),
        ]);
        let scope = Scope::root();
        let page = page(&local, &remote, &query(&scope));

        assert_eq!(
            states(&page),
            [
                ("changed.txt", StateKind::Differs),
                ("conflict", StateKind::TypeConflict),
                ("dir", StateKind::InSync),
                ("extra.txt", StateKind::RemoteOnly),
                ("new.txt", StateKind::MissingRemote),
                ("same.txt", StateKind::InSync),
            ]
        );

        let changed = page
            .entries
            .iter()
            .find(|entry| entry.relative == "changed.txt")
            .unwrap();
        assert_eq!(
            changed.state,
            StatusState::Differs(ChangeReason::MtimeDiffers)
        );
        assert_eq!(changed.local.unwrap().size, 4);
        assert_eq!(changed.remote.unwrap().mtime_seconds, 4);
        assert_eq!(changed.remote_path, "/share/root/changed.txt");
    }

    #[test]
    fn a_type_conflict_names_both_sides() {
        let local = local(&[("conflict", EntryKind::Directory, 0, 0)]);
        let remote = remote(&[("conflict", EntryKind::File, 7, 9)]);
        let scope = Scope::root();
        let page = page(&local, &remote, &query(&scope));
        assert_eq!(
            page.entries[0].state,
            StatusState::TypeConflict {
                local_kind: EntryKind::Directory,
                remote_kind: EntryKind::File,
            }
        );
        assert_eq!(page.stats.type_conflicts, 1);
    }

    #[test]
    fn a_missing_remote_entry_still_carries_the_path_it_would_occupy() {
        let local = local(&[("deep/new.txt", EntryKind::File, 3, 2_000)]);
        let remote = remote(&[]);
        let scope = Scope::root();
        let page = page(&local, &remote, &query(&scope));
        let entry = page
            .entries
            .iter()
            .find(|entry| entry.relative == "deep/new.txt")
            .unwrap();
        assert_eq!(entry.remote_path, "/share/root/deep/new.txt");
        assert_eq!(entry.state, StatusState::MissingRemote);
        assert!(entry.remote.is_none());
    }

    #[test]
    fn the_reported_reason_tracks_the_compare_mode() {
        let local = local(&[("a.txt", EntryKind::File, 4, 5_000)]);
        let remote = remote(&[("a.txt", EntryKind::File, 4, 4)]);
        let scope = Scope::root();

        // Size-only never inspects the differing mtime, so it reports equality.
        let size_only = page(
            &local,
            &remote,
            &StatusQuery {
                compare: CompareMode::SizeOnly,
                ..query(&scope)
            },
        );
        assert_eq!(size_only.entries[0].state, StatusState::InSync);

        let metadata = page(&local, &remote, &query(&scope));
        assert_eq!(
            metadata.entries[0].state,
            StatusState::Differs(ChangeReason::MtimeDiffers)
        );
    }

    #[test]
    fn a_protected_remote_entry_reports_why_it_is_excluded() {
        let local = local(&[]);
        let mut remote = remote(&[("mounted", EntryKind::Directory, 0, 0)]);
        remote.entries.get_mut("mounted").unwrap().mount_point_type = Some("cifs".to_owned());
        let scope = Scope::root();
        let page = page(&local, &remote, &query(&scope));
        assert_eq!(
            page.entries[0].state,
            StatusState::Excluded(ExclusionCause::MountBoundary)
        );
        assert_eq!(page.stats.excluded_entries, 1);
    }

    #[test]
    fn requested_excluded_paths_are_listed_with_their_cause() {
        let excluded: BTreeSet<String> = ["cache/big.tmp".to_owned(), "@eaDir".to_owned()]
            .into_iter()
            .collect();
        let scope = Scope::root();
        let page = build_status_page(
            &root(),
            &local(&[]),
            &remote(&[]),
            &excluded,
            &rules(&[]),
            &StatusQuery {
                include_excluded: true,
                ..query(&scope)
            },
        )
        .unwrap();
        assert_eq!(
            states(&page),
            [
                ("@eaDir", StateKind::Excluded),
                ("cache/big.tmp", StateKind::Excluded),
            ]
        );
        assert_eq!(
            page.entries[0].state,
            StatusState::Excluded(ExclusionCause::DsmManaged)
        );
        assert_eq!(
            page.entries[1].state,
            StatusState::Excluded(ExclusionCause::IgnoreRule)
        );
    }

    #[test]
    fn excluded_paths_stay_hidden_unless_they_are_requested() {
        let excluded: BTreeSet<String> = ["cache/big.tmp".to_owned()].into_iter().collect();
        let scope = Scope::root();
        let page = build_status_page(
            &root(),
            &local(&[]),
            &remote(&[]),
            &excluded,
            &rules(&[]),
            &query(&scope),
        )
        .unwrap();
        assert!(page.entries.is_empty());
    }

    // --- Scoping the listing ---------------------------------------------

    #[test]
    fn a_directory_scope_lists_only_its_subtree() {
        let local = local(&[
            ("docs", EntryKind::Directory, 0, 0),
            ("docs/a.txt", EntryKind::File, 1, 1_000),
            ("docs/deep/b.txt", EntryKind::File, 1, 1_000),
            ("other/c.txt", EntryKind::File, 1, 1_000),
        ]);
        let remote = remote(&[]);
        let scope = Scope::parse("docs").unwrap();
        let page = page(&local, &remote, &query(&scope));
        assert_eq!(names(&page), ["docs", "docs/a.txt", "docs/deep/b.txt"]);
        assert_eq!(page.scope, "docs");
    }

    #[test]
    fn a_file_scope_lists_exactly_one_row_and_omits_its_parents() {
        let local = local(&[
            ("docs", EntryKind::Directory, 0, 0),
            ("docs/a.txt", EntryKind::File, 1, 1_000),
            ("docs/b.txt", EntryKind::File, 1, 1_000),
        ]);
        let remote = remote(&[]);
        let scope = Scope::parse("docs/a.txt").unwrap();
        let page = page(&local, &remote, &query(&scope));
        assert_eq!(names(&page), ["docs/a.txt"]);
        // The parent was scanned so a scoped plan could create it, but it is not in scope and
        // must not appear in the listing or the totals.
        assert_eq!(page.stats.total_entries, 1);
        assert_eq!(page.stats.directories, 0);
    }

    #[test]
    fn a_scoped_listing_ignores_remote_entries_outside_the_scope() {
        let local = local(&[("docs/a.txt", EntryKind::File, 1, 1_000)]);
        let remote = remote(&[
            ("docs/a.txt", EntryKind::File, 1, 1),
            ("elsewhere/orphan.txt", EntryKind::File, 5, 5),
        ]);
        let scope = Scope::parse("docs").unwrap();
        let page = page(&local, &remote, &query(&scope));
        assert_eq!(names(&page), ["docs/a.txt"]);
        assert_eq!(page.stats.remote_only_entries, 0);
    }

    // --- Stats ------------------------------------------------------------

    #[test]
    fn stats_agree_with_the_plan_built_from_the_same_inventories() {
        let local = local(&[
            ("dir", EntryKind::Directory, 0, 0),
            ("same.txt", EntryKind::File, 4, 3_500),
            ("changed.txt", EntryKind::File, 40, 5_000),
            ("new.txt", EntryKind::File, 300, 2_000),
        ]);
        let remote = remote(&[
            ("dir", EntryKind::Directory, 0, 0),
            ("same.txt", EntryKind::File, 4, 3),
            ("changed.txt", EntryKind::File, 40, 4),
            ("extra.txt", EntryKind::File, 1, 1),
        ]);
        let scope = Scope::root();
        let page = page(&local, &remote, &query(&scope));
        let plan = build_plan(
            &root(),
            &local,
            &remote,
            &rules(&[]),
            &PlanOptions {
                delete: true,
                allow_empty_source: false,
                max_delete: 100,
                compare: CompareMode::Metadata,
                server_copy: false,
                scope: Scope::root(),
            },
        )
        .unwrap();

        // The two must never drift: they answer the same question from the same comparison.
        assert_eq!(page.stats.in_sync_files, plan.unchanged_files);
        assert_eq!(page.stats.transfer_bytes, plan.upload_bytes);
        assert_eq!(
            page.stats.differing_files + page.stats.missing_remote_files,
            plan.uploads.len()
        );
        assert_eq!(page.stats.remote_only_entries, plan.delete_count());
        assert_eq!(page.stats.transfer_bytes, 340);
        assert_eq!(page.stats.in_sync_bytes, 4);
    }

    #[test]
    fn filters_narrow_the_rows_but_never_the_totals() {
        let local = local(&[
            ("same.txt", EntryKind::File, 4, 3_500),
            ("changed.txt", EntryKind::File, 40, 5_000),
        ]);
        let remote = remote(&[
            ("same.txt", EntryKind::File, 4, 3),
            ("changed.txt", EntryKind::File, 40, 4),
        ]);
        let scope = Scope::root();
        let page = page(
            &local,
            &remote,
            &StatusQuery {
                states: StateFilter::Only(vec![StateKind::Differs]),
                ..query(&scope)
            },
        );
        assert_eq!(states(&page), [("changed.txt", StateKind::Differs)]);
        // A caller showing only the differing row still learns what sits behind it.
        assert_eq!(page.stats.in_sync_files, 1);
        assert_eq!(page.stats.differing_files, 1);
        assert_eq!(page.stats.total_entries, 2);
    }

    #[test]
    fn the_attention_set_is_the_states_that_need_action() {
        assert_eq!(
            StateKind::ATTENTION,
            &[
                StateKind::TypeConflict,
                StateKind::MissingRemote,
                StateKind::Differs,
                StateKind::RemoteOnly,
            ]
        );
        assert!(StateKind::TypeConflict.needs_attention());
        assert!(StateKind::MissingRemote.needs_attention());
        assert!(StateKind::Differs.needs_attention());
        assert!(StateKind::RemoteOnly.needs_attention());
        assert!(!StateKind::InSync.needs_attention());
        assert!(!StateKind::Excluded.needs_attention());
    }

    #[test]
    fn the_attention_filter_returns_exactly_the_attention_states() {
        let local = local(&[
            ("same.txt", EntryKind::File, 4, 3_500),
            ("changed.txt", EntryKind::File, 4, 5_000),
            ("new.txt", EntryKind::File, 3, 2_000),
        ]);
        let remote = remote(&[
            ("same.txt", EntryKind::File, 4, 3),
            ("changed.txt", EntryKind::File, 4, 4),
            ("extra.txt", EntryKind::File, 1, 1),
        ]);
        let scope = Scope::root();
        let page = page(
            &local,
            &remote,
            &StatusQuery {
                states: StateFilter::attention(),
                ..query(&scope)
            },
        );
        assert_eq!(
            states(&page),
            [
                ("changed.txt", StateKind::Differs),
                ("extra.txt", StateKind::RemoteOnly),
                ("new.txt", StateKind::MissingRemote),
            ]
        );
        assert_eq!(page.stats.attention_entries, 3);
        assert_eq!(page.stats.in_sync_files, 1);
    }

    #[test]
    fn the_name_filter_matches_the_last_component_case_insensitively() {
        let local = local(&[
            ("docs/Invoice-1.pdf", EntryKind::File, 1, 1_000),
            ("invoices/other.txt", EntryKind::File, 1, 1_000),
            ("docs/report.pdf", EntryKind::File, 1, 1_000),
        ]);
        let remote = remote(&[]);
        let scope = Scope::root();
        let page = page(
            &local,
            &remote,
            &StatusQuery {
                filter: Some("invoice"),
                ..query(&scope)
            },
        );
        // Matches the file name, not the directory that happens to contain the word.
        assert_eq!(names(&page), ["docs/Invoice-1.pdf"]);
        assert_eq!(page.stats.total_entries, 3);
    }

    #[test]
    fn an_empty_filter_is_ignored_rather_than_matching_nothing() {
        let local = local(&[("a.txt", EntryKind::File, 1, 1_000)]);
        let remote = remote(&[]);
        let scope = Scope::root();
        let page = page(
            &local,
            &remote,
            &StatusQuery {
                filter: Some("   "),
                ..query(&scope)
            },
        );
        assert_eq!(page.entries.len(), 1);
    }

    // --- Pagination -------------------------------------------------------

    fn numbered(count: usize) -> LocalInventory {
        let owned: Vec<String> = (0..count).map(|index| format!("f{index:04}.txt")).collect();
        let entries: Vec<(&str, EntryKind, u64, i64)> = owned
            .iter()
            .map(|relative| (relative.as_str(), EntryKind::File, 1, 1_000))
            .collect();
        local(&entries)
    }

    #[test]
    fn the_page_size_is_capped_no_matter_what_the_caller_asks_for() {
        let local = numbered(250);
        let remote = remote(&[]);
        let scope = Scope::root();
        let page = page(
            &local,
            &remote,
            &StatusQuery {
                limit: usize::MAX,
                ..query(&scope)
            },
        );
        assert_eq!(page.limit, STATUS_PAGE_SIZE_MAX);
        assert_eq!(page.entries.len(), STATUS_PAGE_SIZE_MAX);
        assert!(page.truncated);
        // The totals still describe the whole scope, not the capped page.
        assert_eq!(page.stats.total_entries, 250);
    }

    #[test]
    fn a_zero_limit_still_returns_a_usable_page() {
        let local = numbered(3);
        let remote = remote(&[]);
        let scope = Scope::root();
        let page = page(
            &local,
            &remote,
            &StatusQuery {
                limit: 0,
                ..query(&scope)
            },
        );
        assert_eq!(page.limit, 1);
        assert_eq!(page.entries.len(), 1);
    }

    #[test]
    fn a_page_that_exactly_fits_reports_no_continuation() {
        let local = numbered(5);
        let remote = remote(&[]);
        let scope = Scope::root();
        for (limit, truncated) in [(4, true), (5, false), (6, false)] {
            let current = page(
                &local,
                &remote,
                &StatusQuery {
                    limit,
                    ..query(&scope)
                },
            );
            assert_eq!(current.truncated, truncated, "limit {limit}");
            assert_eq!(current.next_cursor.is_some(), truncated, "limit {limit}");
        }
    }

    #[test]
    fn paging_with_the_cursor_visits_every_entry_exactly_once() {
        let local = numbered(23);
        let remote = remote(&[]);
        let scope = Scope::root();
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let current = build_status_page(
                &root(),
                &local,
                &remote,
                &BTreeSet::new(),
                &rules(&[]),
                &StatusQuery {
                    limit: 5,
                    cursor: cursor.as_ref(),
                    ..query(&scope)
                },
            )
            .unwrap();
            seen.extend(current.entries.iter().map(|entry| entry.relative.clone()));
            match current.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let expected: Vec<String> = (0..23).map(|index| format!("f{index:04}.txt")).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_cursor_naming_a_vanished_entry_still_resumes_in_order() {
        // A cursor is a position, not a lookup: the engine keeps no snapshot to invalidate.
        let local = local(&[
            ("a.txt", EntryKind::File, 1, 1_000),
            ("c.txt", EntryKind::File, 1, 1_000),
        ]);
        let remote = remote(&[]);
        let scope = Scope::root();
        let cursor = StatusCursor::new("b.txt");
        let page = page(
            &local,
            &remote,
            &StatusQuery {
                cursor: Some(&cursor),
                ..query(&scope)
            },
        );
        assert_eq!(names(&page), ["c.txt"]);
    }

    #[test]
    fn the_cursor_survives_a_filtered_listing() {
        let local = local(&[
            ("a-differs.txt", EntryKind::File, 4, 5_000),
            ("b-same.txt", EntryKind::File, 4, 3_500),
            ("c-differs.txt", EntryKind::File, 4, 5_000),
        ]);
        let remote = remote(&[
            ("a-differs.txt", EntryKind::File, 4, 4),
            ("b-same.txt", EntryKind::File, 4, 3),
            ("c-differs.txt", EntryKind::File, 4, 4),
        ]);
        let scope = Scope::root();
        let first = page(
            &local,
            &remote,
            &StatusQuery {
                limit: 1,
                states: StateFilter::Only(vec![StateKind::Differs]),
                ..query(&scope)
            },
        );
        assert_eq!(states(&first), [("a-differs.txt", StateKind::Differs)]);
        let cursor = first.next_cursor.expect("a second differing row remains");

        let second = page(
            &local,
            &remote,
            &StatusQuery {
                limit: 1,
                states: StateFilter::Only(vec![StateKind::Differs]),
                cursor: Some(&cursor),
                ..query(&scope)
            },
        );
        assert_eq!(states(&second), [("c-differs.txt", StateKind::Differs)]);
        assert!(!second.truncated);
    }

    #[test]
    fn stats_are_complete_by_default_and_the_page_echoes_its_scope() {
        let local = numbered(2);
        let remote = remote(&[]);
        let scope = Scope::root();
        let page = page(&local, &remote, &query(&scope));
        assert!(page.stats.complete);
        assert_eq!(page.scope, "");
        assert_eq!(page.limit, STATUS_PAGE_SIZE_DEFAULT);
    }

    #[test]
    fn state_kind_names_are_stable_for_machine_output() {
        assert_eq!(StateKind::TypeConflict.as_str(), "type-conflict");
        assert_eq!(StateKind::MissingRemote.as_str(), "missing-remote");
        assert_eq!(StateKind::Differs.as_str(), "differs");
        assert_eq!(StateKind::RemoteOnly.as_str(), "remote-only");
        assert_eq!(StateKind::InSync.as_str(), "in-sync");
        assert_eq!(StateKind::Excluded.as_str(), "excluded");
        assert_eq!(ExclusionCause::IgnoreRule.as_str(), "ignore-rule");
        assert_eq!(ExclusionCause::DsmManaged.as_str(), "dsm-managed");
        assert_eq!(ExclusionCause::MountBoundary.as_str(), "mount-boundary");
    }

    #[test]
    fn a_status_state_exposes_the_planner_reason_behind_it() {
        assert_eq!(
            StatusState::Differs(ChangeReason::SizeDiffers).reason(),
            Some(ChangeReason::SizeDiffers)
        );
        assert_eq!(
            StatusState::MissingRemote.reason(),
            Some(ChangeReason::MissingRemote)
        );
        assert_eq!(StatusState::InSync.reason(), None);
        assert_eq!(StatusState::RemoteOnly.reason(), None);
    }
}

#[cfg(test)]
mod scoped_deletion_tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn root() -> RemoteRoot {
        RemoteRoot::parse("/share/root").unwrap()
    }

    fn local(entries: &[(&str, EntryKind, u64, i64)]) -> LocalInventory {
        LocalInventory {
            root: PathBuf::from("/source"),
            entries: entries
                .iter()
                .map(|(relative, kind, size, mtime_ms)| {
                    (
                        (*relative).to_owned(),
                        LocalEntry {
                            relative: (*relative).to_owned(),
                            full_path: PathBuf::from(relative),
                            kind: *kind,
                            size: *size,
                            mtime_ms: *mtime_ms,
                            identity: Default::default(),
                            content_md5: None,
                        },
                    )
                })
                .collect(),
            skipped: Vec::new(),
        }
    }

    fn remote(entries: &[(&str, EntryKind, u64, i64)]) -> RemoteInventory {
        RemoteInventory {
            root_exists: true,
            entries: entries
                .iter()
                .map(|(relative, kind, size, mtime_seconds)| {
                    (
                        (*relative).to_owned(),
                        RemoteEntry {
                            relative: (*relative).to_owned(),
                            remote_path: format!("/share/root/{relative}"),
                            kind: *kind,
                            size: *size,
                            mtime_seconds: *mtime_seconds,
                            mount_point_type: None,
                            content_md5: None,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn rules() -> IgnoreRules {
        let root = std::env::temp_dir().join(format!("sdsync-scoped-del-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        IgnoreRules::build(&root, &[]).unwrap()
    }

    fn mirror(scope: Scope) -> PlanOptions {
        PlanOptions {
            delete: true,
            allow_empty_source: true,
            max_delete: 100,
            compare: CompareMode::Metadata,
            server_copy: false,
            scope,
        }
    }

    fn deleted(plan: &SyncPlan) -> Vec<&str> {
        plan.post_deletes
            .iter()
            .map(|action| action.relative.as_str())
            .collect()
    }

    #[test]
    fn a_scoped_mirror_never_deletes_the_ancestors_that_lead_to_its_scope() {
        // A scoped remote inventory carries the directories between the root and the scope so a
        // scoped upload's parents are known to exist. They have no local counterpart to match,
        // so without the scope restriction they would read as remote-only and be deleted.
        let local = local(&[
            ("docs", EntryKind::Directory, 0, 0),
            ("docs/q3", EntryKind::Directory, 0, 0),
            ("docs/q3/keep.txt", EntryKind::File, 4, 1_000),
        ]);
        let remote = remote(&[
            ("docs", EntryKind::Directory, 0, 0),
            ("docs/q3", EntryKind::Directory, 0, 0),
            ("docs/q3/keep.txt", EntryKind::File, 4, 1),
            ("docs/q3/stale.txt", EntryKind::File, 9, 2),
        ]);
        let plan = build_plan(
            &root(),
            &local,
            &remote,
            &rules(),
            &mirror(Scope::parse("docs/q3").unwrap()),
        )
        .unwrap();
        assert_eq!(deleted(&plan), ["docs/q3/stale.txt"]);
    }

    #[test]
    fn a_scoped_mirror_ignores_remote_only_entries_outside_the_scope() {
        // The rest of the destination is not the scoped run's business, even under --delete.
        let local = local(&[("docs/q3/keep.txt", EntryKind::File, 4, 1_000)]);
        let remote = remote(&[
            ("docs/q3/keep.txt", EntryKind::File, 4, 1),
            ("elsewhere", EntryKind::Directory, 0, 0),
            ("elsewhere/orphan.txt", EntryKind::File, 9, 2),
            ("top-level.txt", EntryKind::File, 3, 3),
        ]);
        let plan = build_plan(
            &root(),
            &local,
            &remote,
            &rules(),
            &mirror(Scope::parse("docs/q3").unwrap()),
        )
        .unwrap();
        assert!(
            plan.post_deletes.is_empty(),
            "a scoped mirror proposed out-of-scope deletions: {:?}",
            deleted(&plan)
        );
    }

    #[test]
    fn a_single_file_scope_deletes_nothing_but_that_file() {
        let local = local(&[]);
        let remote = remote(&[
            ("docs/a.txt", EntryKind::File, 1, 1),
            ("docs/b.txt", EntryKind::File, 1, 1),
        ]);
        let plan = build_plan(
            &root(),
            &local,
            &remote,
            &rules(),
            &mirror(Scope::parse("docs/a.txt").unwrap()),
        )
        .unwrap();
        assert_eq!(deleted(&plan), ["docs/a.txt"]);
    }

    #[test]
    fn an_unscoped_mirror_still_considers_the_whole_destination() {
        let local = local(&[("docs/q3/keep.txt", EntryKind::File, 4, 1_000)]);
        let remote = remote(&[
            ("docs/q3/keep.txt", EntryKind::File, 4, 1),
            ("elsewhere/orphan.txt", EntryKind::File, 9, 2),
        ]);
        let plan = build_plan(&root(), &local, &remote, &rules(), &mirror(Scope::root())).unwrap();
        assert_eq!(deleted(&plan), ["elsewhere/orphan.txt"]);
    }

    #[test]
    fn the_delete_limit_counts_only_in_scope_deletions() {
        let local = local(&[]);
        let remote = remote(&[
            ("docs/a.txt", EntryKind::File, 1, 1),
            ("elsewhere/b.txt", EntryKind::File, 1, 1),
            ("elsewhere/c.txt", EntryKind::File, 1, 1),
            ("elsewhere/d.txt", EntryKind::File, 1, 1),
        ]);
        // A cap of one would trip if the three out-of-scope entries were counted.
        let plan = build_plan(
            &root(),
            &local,
            &remote,
            &rules(),
            &PlanOptions {
                max_delete: 1,
                ..mirror(Scope::parse("docs").unwrap())
            },
        )
        .unwrap();
        assert_eq!(deleted(&plan), ["docs/a.txt"]);
    }
}

#[cfg(test)]
mod resync_ticket_tests {
    use std::path::PathBuf;

    use super::*;

    fn upload(relative: &str, size: u64) -> UploadAction {
        UploadAction {
            local: LocalEntry {
                relative: relative.to_owned(),
                full_path: PathBuf::from(relative),
                kind: EntryKind::File,
                size,
                mtime_ms: 1_000,
                identity: Default::default(),
                content_md5: None,
            },
            remote_path: format!("/share/root/{relative}"),
            reason: ChangeReason::Forced,
            expected_remote: None,
        }
    }

    fn plan_of(uploads: Vec<UploadAction>) -> SyncPlan {
        let upload_bytes = uploads
            .iter()
            .fold(0_u64, |total, action| total + action.local.size);
        SyncPlan {
            source_root: PathBuf::from("/srv/exports"),
            scope: Scope::root(),
            rules: IgnoreRules::none(),
            pre_deletes: Vec::new(),
            creates: Vec::new(),
            copies: Vec::new(),
            uploads,
            post_deletes: Vec::new(),
            unchanged_files: 0,
            protected_entries: 0,
            upload_bytes,
            skipped: Vec::new(),
        }
    }

    const TARGET: ResyncTarget<'static> = ResyncTarget {
        remote_root: "/share/root",
        endpoint: "https://nas.example.invalid/",
    };

    fn ticket(plan: &SyncPlan) -> String {
        resync_ticket(TARGET, &Scope::root(), CompareMode::Force, plan)
    }

    #[test]
    fn a_ticket_is_short_stable_and_hex() {
        let plan = plan_of(vec![upload("a.txt", 10), upload("b.txt", 20)]);
        let first = ticket(&plan);
        assert_eq!(first.len(), RESYNC_TICKET_LENGTH);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
        // Same work, same ticket: confirming is not a race against the clock.
        assert_eq!(first, ticket(&plan));
    }

    #[test]
    fn an_unchanged_plan_keeps_its_ticket_no_matter_how_much_time_passes() {
        // Nothing in the encoding is time-derived, so a ticket cannot expire on its own. The only
        // thing that invalidates one is a change to what would be overwritten.
        let plan = plan_of(vec![upload("a.txt", 10)]);
        let before = ticket(&plan);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(before, ticket(&plan));
    }

    #[test]
    fn adding_a_file_to_the_overwrite_set_invalidates_the_ticket() {
        let original = plan_of(vec![upload("a.txt", 10)]);
        let grown = plan_of(vec![upload("a.txt", 10), upload("b.txt", 20)]);
        assert_ne!(ticket(&original), ticket(&grown));
    }

    #[test]
    fn removing_a_file_from_the_overwrite_set_invalidates_the_ticket() {
        let original = plan_of(vec![upload("a.txt", 10), upload("b.txt", 20)]);
        let shrunk = plan_of(vec![upload("a.txt", 10)]);
        assert_ne!(ticket(&original), ticket(&shrunk));
    }

    #[test]
    fn a_file_changing_size_invalidates_the_ticket() {
        // The byte total is what the caller was shown, so it is part of what they confirmed.
        let original = plan_of(vec![upload("a.txt", 10)]);
        let resized = plan_of(vec![upload("a.txt", 11)]);
        assert_ne!(ticket(&original), ticket(&resized));
    }

    #[test]
    fn swapping_which_file_is_overwritten_invalidates_the_ticket() {
        // Same count and same byte total, different files: the ticket must still differ, or a
        // confirmation would authorize overwriting something the caller never saw.
        let original = plan_of(vec![upload("a.txt", 10)]);
        let swapped = plan_of(vec![upload("b.txt", 10)]);
        assert_ne!(ticket(&original), ticket(&swapped));
    }

    #[test]
    fn a_different_scope_invalidates_the_ticket() {
        let plan = plan_of(vec![upload("docs/a.txt", 10)]);
        let broad = resync_ticket(TARGET, &Scope::root(), CompareMode::Force, &plan);
        let narrow = resync_ticket(
            TARGET,
            &Scope::parse("docs").unwrap(),
            CompareMode::Force,
            &plan,
        );
        assert_ne!(
            broad, narrow,
            "a ticket from one scope must not confirm a run of another"
        );
    }

    #[test]
    fn a_different_comparison_invalidates_the_ticket() {
        let plan = plan_of(vec![upload("a.txt", 10)]);
        assert_ne!(
            resync_ticket(TARGET, &Scope::root(), CompareMode::Force, &plan),
            resync_ticket(TARGET, &Scope::root(), CompareMode::Content, &plan)
        );
    }

    #[test]
    fn a_different_destination_root_invalidates_the_ticket() {
        // The overwrite set in `Force` mode is a function of local state alone, so without the
        // destination in the digest two profiles pointing one source at two folders would issue
        // the same ticket and either could confirm the other.
        let plan = plan_of(vec![upload("report.pdf", 1_024)]);
        let team = resync_ticket(
            ResyncTarget {
                remote_root: "/team/docs",
                ..TARGET
            },
            &Scope::root(),
            CompareMode::Force,
            &plan,
        );
        let archive = resync_ticket(
            ResyncTarget {
                remote_root: "/archive/docs",
                ..TARGET
            },
            &Scope::root(),
            CompareMode::Force,
            &plan,
        );
        assert_ne!(
            team, archive,
            "a ticket for one destination must not confirm an overwrite of another"
        );
    }

    #[test]
    fn a_different_endpoint_invalidates_the_ticket() {
        // Same source, same folder, different NAS: replaying the ticket against the second unit
        // overwrites files nobody was shown.
        let plan = plan_of(vec![upload("report.pdf", 1_024)]);
        let first = resync_ticket(TARGET, &Scope::root(), CompareMode::Force, &plan);
        let second = resync_ticket(
            ResyncTarget {
                endpoint: "https://other-nas.example.invalid/",
                ..TARGET
            },
            &Scope::root(),
            CompareMode::Force,
            &plan,
        );
        assert_ne!(first, second);
    }

    #[test]
    fn a_version_one_ticket_no_longer_confirms_anything() {
        // Reconstructed exactly as the previous encoding built it: the same fields in the same
        // order under the v1 domain tag. Nothing that hashed to a v1 ticket may still be accepted,
        // because a v1 ticket makes a claim about the destination it never covered.
        let plan = plan_of(vec![upload("a.txt", 10)]);
        let mut canonical = String::from("sdsync.resync-ticket.v1\n");
        canonical.push_str(Scope::root().as_str());
        canonical.push('\n');
        canonical.push_str("force\n");
        canonical.push_str(&plan.upload_bytes.to_string());
        canonical.push('\n');
        for action in &plan.uploads {
            canonical.push_str(&action.local.relative);
            canonical.push('\0');
            canonical.push_str(&action.local.size.to_string());
            canonical.push('\n');
        }
        let mut legacy = ContentMd5::from_content(canonical.as_bytes())
            .sha256_hex()
            .expect("a freshly computed fingerprint carries its SHA-256");
        legacy.truncate(RESYNC_TICKET_LENGTH);

        assert_eq!(legacy.len(), RESYNC_TICKET_LENGTH);
        assert_ne!(
            legacy,
            ticket(&plan),
            "a version 1 ticket must be reported stale rather than honoured"
        );
    }

    /// A version 2 ticket no longer confirms anything either.
    ///
    /// It covered the destination but not the bytes, and `resync` is the one command that destroys
    /// remote revisions unconditionally. The tag is inside the hashed string, so a v2 ticket simply
    /// fails to match and is reported stale with the refreshed plan.
    #[test]
    fn a_version_two_ticket_no_longer_confirms_anything() {
        let plan = plan_of(vec![upload_with_content("a.txt", b"original")]);
        let mut canonical = String::from("sdsync.resync-ticket.v2\n");
        canonical.push_str(TARGET.endpoint);
        canonical.push('\n');
        canonical.push_str(TARGET.remote_root);
        canonical.push('\n');
        canonical.push_str(Scope::root().as_str());
        canonical.push('\n');
        canonical.push_str("force\n");
        canonical.push_str(&plan.upload_bytes.to_string());
        canonical.push('\n');
        for action in &plan.uploads {
            canonical.push_str(&action.local.relative);
            canonical.push('\0');
            canonical.push_str(&action.local.size.to_string());
            canonical.push('\n');
        }
        let mut legacy = ContentMd5::from_content(canonical.as_bytes())
            .sha256_hex()
            .expect("a freshly computed fingerprint carries its SHA-256");
        legacy.truncate(RESYNC_TICKET_LENGTH);

        assert_ne!(legacy, ticket(&plan));
    }

    /// The scenario the ticket's own documentation claimed to cover and did not: the same path, the
    /// same length, different bytes.
    ///
    /// The operator is shown "overwrite `summary.pdf`, 1,024,000 bytes" and handed a ticket. Before
    /// they confirm, the file is replaced by a regenerated report, a restored older revision, or the
    /// wrong file dropped in by a colleague, at the same length. The ticket still matched, and
    /// `resync --confirm` overwrote the remote copy — and every revision DSM held of it — with
    /// content nobody reviewed.
    #[test]
    fn a_same_length_content_change_invalidates_the_ticket() {
        let reviewed = plan_of(vec![upload_with_content("summary.pdf", b"reviewed")]);
        let substituted = plan_of(vec![upload_with_content("summary.pdf", b"replaced")]);

        assert_eq!(
            reviewed.uploads[0].local.size, substituted.uploads[0].local.size,
            "the fixture has to hold the length constant for the test to mean anything"
        );
        assert_ne!(ticket(&reviewed), ticket(&substituted));
    }

    /// A digest-less upload encodes as a distinct string rather than as an absence that could be
    /// forged into a digest.
    #[test]
    fn an_upload_with_no_digest_does_not_collide_with_one_that_has_it() {
        let with_digest = plan_of(vec![upload_with_content("a.txt", b"payload!")]);
        let without = plan_of(vec![upload("a.txt", 8)]);

        assert_eq!(
            with_digest.uploads[0].local.size,
            without.uploads[0].local.size
        );
        assert_ne!(ticket(&with_digest), ticket(&without));
    }

    fn upload_with_content(relative: &str, content: &[u8]) -> UploadAction {
        let mut action = upload(relative, content.len() as u64);
        action.local.content_md5 = Some(ContentMd5::from_content(content));
        action
    }

    #[test]
    fn an_empty_plan_still_has_a_ticket() {
        let empty = plan_of(Vec::new());
        assert_eq!(ticket(&empty).len(), RESYNC_TICKET_LENGTH);
    }

    #[test]
    fn path_boundaries_cannot_be_forged_by_concatenation() {
        // Encoding separates path from size with a NUL, so two different sets cannot collide by
        // running their fields together.
        let left = plan_of(vec![upload("ab", 1), upload("c", 1)]);
        let right = plan_of(vec![upload("a", 1), upload("bc", 1)]);
        assert_ne!(ticket(&left), ticket(&right));
    }

    #[test]
    fn the_default_state_filter_is_the_attention_set() {
        // A caller that does not choose gets the view the request is usually for. The command line
        // opts into StateFilter::All explicitly instead.
        assert_eq!(StateFilter::default(), StateFilter::attention());
        assert_eq!(
            StateFilter::default(),
            StateFilter::Only(StateKind::ATTENTION.to_vec())
        );
    }
}
