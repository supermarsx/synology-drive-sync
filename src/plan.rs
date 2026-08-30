use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::api::{RemoteEntry, RemoteInventory};
use crate::integrity::ContentMd5;
use crate::local::{EntryKind, IgnoreRules, LocalEntry, LocalInventory};
use crate::path::{RemoteRoot, depth, is_dsm_managed};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareMode {
    Content,
    Metadata,
    SizeOnly,
}

#[derive(Clone, Debug)]
pub struct PlanOptions {
    pub delete: bool,
    pub allow_empty_source: bool,
    pub max_delete: usize,
    pub compare: CompareMode,
    pub server_copy: bool,
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
    pub pre_deletes: Vec<DeleteAction>,
    pub creates: Vec<CreateAction>,
    pub copies: Vec<CopyAction>,
    pub uploads: Vec<UploadAction>,
    pub post_deletes: Vec<DeleteAction>,
    pub unchanged_files: usize,
    pub protected_entries: usize,
    pub upload_bytes: u64,
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

/// Select remote content needed for comparison, optional server-copy reuse, and deletion guards.
/// A content-mode mirror must pass `delete = true` so every file that could be removed has a
/// plan-time digest. `build_plan` fails closed if such a digest is absent.
pub fn select_remote_content_hashes_for_plan(
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    server_copy: bool,
    delete: bool,
) -> BTreeSet<String> {
    let mut selected = BTreeSet::new();
    for local_entry in local.entries.values() {
        if local_entry.kind != EntryKind::File {
            continue;
        }
        let Some(remote_entry) = remote.entries.get(&local_entry.relative) else {
            continue;
        };
        if remote_entry.kind == EntryKind::File
            && remote_entry.size == local_entry.size
            && local_mtime_seconds(local_entry) == remote_entry.mtime_seconds
            && !is_protected(remote_entry, rules)
        {
            selected.insert(remote_entry.relative.clone());
        }
    }

    if delete {
        for entry in remote.entries.values() {
            if entry.kind != EntryKind::File || is_protected(entry, rules) {
                continue;
            }
            let is_delete_candidate = local
                .entries
                .get(&entry.relative)
                .is_none_or(|local_entry| local_entry.kind != EntryKind::File);
            if is_delete_candidate {
                selected.insert(entry.relative.clone());
            }
        }
    }

    if !server_copy {
        return selected;
    }

    let mut missing_by_size_name_and_mtime: BTreeMap<(u64, String, i64), BTreeSet<String>> =
        BTreeMap::new();
    for entry in local.entries.values() {
        if entry.kind != EntryKind::File || remote.entries.contains_key(&entry.relative) {
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
            || local.entries.contains_key(&entry.relative)
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
    validate_portable_case_mapping(root, local, remote)?;

    let mut plan = SyncPlan {
        pre_deletes: Vec::new(),
        creates: Vec::new(),
        copies: Vec::new(),
        uploads: Vec::new(),
        post_deletes: Vec::new(),
        unchanged_files: 0,
        protected_entries: 0,
        upload_bytes: 0,
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
            .any(|relative| relative.starts_with(&prefix))
        {
            return Err(Error::ProtectedConflict(mount.remote_path.clone()));
        }
    }

    let mut predeleted = BTreeSet::new();
    for entry in local.entries.values() {
        let Some(remote_entry) = remote.entries.get(&entry.relative) else {
            schedule_missing(root, &mut plan, entry)?;
            continue;
        };

        match (entry.kind, remote_entry.kind) {
            (EntryKind::Directory, EntryKind::Directory) => {}
            (EntryKind::File, EntryKind::File) => {
                match compare_files(entry, remote_entry, options.compare) {
                    None => plan.unchanged_files += 1,
                    Some(reason) => schedule_upload(root, &mut plan, entry, reason)?,
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
                schedule_create(root, &mut plan, entry, ChangeReason::TypeReplaced)?;
            }
            (EntryKind::File, EntryKind::Directory) => {
                let subtree = remote_subtree(&remote.entries, &entry.relative);
                if subtree
                    .iter()
                    .any(|candidate| is_protected(candidate, rules))
                {
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
                schedule_upload(root, &mut plan, entry, ChangeReason::TypeReplaced)?;
            }
        }
    }

    if options.compare == CompareMode::Content {
        replace_uploads_with_server_copies(root, &mut plan, local, remote, rules, options)?;
    }

    if options.delete {
        let mut protected_dirs = HashSet::new();
        for entry in remote.entries.values() {
            if is_protected(entry, rules) {
                plan.protected_entries += 1;
                add_ancestor_directories(&entry.relative, &mut protected_dirs);
            }
        }

        for entry in remote.entries.values() {
            if local.entries.contains_key(&entry.relative) || predeleted.contains(&entry.relative) {
                continue;
            }
            if is_protected(entry, rules)
                || (entry.kind == EntryKind::Directory && protected_dirs.contains(&entry.relative))
            {
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

fn validate_portable_case_mapping(
    root: &RemoteRoot,
    local: &LocalInventory,
    remote: &RemoteInventory,
) -> Result<()> {
    let mut seen = BTreeMap::<String, String>::new();
    for relative in local.entries.keys().chain(remote.entries.keys()) {
        let mut prefix = String::new();
        for component in relative.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let folded = prefix.to_lowercase();
            if let Some(first) = seen.get(&folded)
                && first != &prefix
            {
                return Err(Error::UnsafeRemotePath {
                    path: format!("{}/{}", root.as_str(), prefix),
                    reason: format!(
                        "{first:?} and {prefix:?} differ only by case and cannot coexist portably on Synology Drive clients"
                    ),
                });
            }
            seen.entry(folded).or_insert_with(|| prefix.clone());
        }
    }
    Ok(())
}

fn schedule_missing(root: &RemoteRoot, plan: &mut SyncPlan, entry: &LocalEntry) -> Result<()> {
    match entry.kind {
        EntryKind::Directory => schedule_create(root, plan, entry, ChangeReason::MissingRemote),
        EntryKind::File => schedule_upload(root, plan, entry, ChangeReason::MissingRemote),
    }
}

fn schedule_create(
    root: &RemoteRoot,
    plan: &mut SyncPlan,
    entry: &LocalEntry,
    reason: ChangeReason,
) -> Result<()> {
    plan.creates.push(CreateAction {
        relative: entry.relative.clone(),
        remote_path: root.join(&entry.relative)?,
        reason,
    });
    Ok(())
}

fn schedule_upload(
    root: &RemoteRoot,
    plan: &mut SyncPlan,
    entry: &LocalEntry,
    reason: ChangeReason,
) -> Result<()> {
    plan.upload_bytes = plan.upload_bytes.saturating_add(entry.size);
    plan.uploads.push(UploadAction {
        local: entry.clone(),
        remote_path: root.join(&entry.relative)?,
        reason,
    });
    Ok(())
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
    if local.size != remote.size {
        return Some(ChangeReason::SizeDiffers);
    }
    let mtime_differs = local_mtime_seconds(local) != remote.mtime_seconds;
    match mode {
        CompareMode::SizeOnly => None,
        CompareMode::Metadata => mtime_differs.then_some(ChangeReason::MtimeDiffers),
        CompareMode::Content => match (local.content_md5, remote.content_md5) {
            (Some(local_digest), Some(remote_digest)) => {
                match local_digest.full_match(&remote_digest) {
                    Some(true) => mtime_differs.then_some(ChangeReason::MtimeDiffers),
                    Some(false) => Some(ChangeReason::ContentDiffers),
                    None if mtime_differs => Some(ChangeReason::MtimeDiffers),
                    None if !local_digest.has_full_proof() => {
                        Some(ChangeReason::LocalDigestUnavailable)
                    }
                    None => Some(ChangeReason::RemoteDigestUnavailable),
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
    root: &RemoteRoot,
    plan: &mut SyncPlan,
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    options: &PlanOptions,
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
        if remote.entries.contains_key(&upload.local.relative) {
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
            || local.entries.contains_key(&entry.relative)
            || is_protected(entry, rules)
            || predeleted.contains(entry.relative.as_str())
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
                to_remote_path: root.join(&upload.local.relative)?,
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
    let prefix = format!("{relative}/");
    entries
        .values()
        .filter(|entry| entry.relative == relative || entry.relative.starts_with(&prefix))
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

fn deletion_snapshot(
    entry: &RemoteEntry,
    remote_entries: &BTreeMap<String, RemoteEntry>,
    compare: CompareMode,
) -> Result<RemoteSnapshot> {
    let content_md5 = if compare == CompareMode::Content && entry.kind == EntryKind::File {
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
    let has_descendants = entry.kind == EntryKind::Directory
        && remote_entries
            .keys()
            .any(|relative| relative.starts_with(&format!("{}/", entry.relative)));
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
                            content_md5: None,
                        },
                    )
                })
                .collect(),
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
        }
    }

    fn digest(value: u8) -> ContentMd5 {
        ContentMd5::from_digests([value; 16], u32::from(value), [value; 32])
    }

    fn content_options(delete: bool, server_copy: bool) -> PlanOptions {
        PlanOptions {
            compare: CompareMode::Content,
            server_copy,
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

    #[test]
    fn content_mode_fails_closed_for_md5_only_legacy_values() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let legacy = ContentMd5::from_bytes([0x33; 16]);
        let complete = ContentMd5::from_digests([0x33; 16], 0x1122_3344, [0x44; 32]);
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

        assert_eq!(
            plan_for(legacy, complete).uploads[0].reason,
            ChangeReason::LocalDigestUnavailable
        );
        assert_eq!(
            plan_for(complete, legacy).uploads[0].reason,
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

    #[test]
    fn rejects_case_variant_paths_across_and_within_remote_inventory() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let local_inventory = local(&[
            ("folder", EntryKind::Directory, 0, 0),
            ("folder/local.bin", EntryKind::File, 1, 1_000),
        ]);
        let remote_inventory = remote(&[
            ("Folder", EntryKind::Directory, 0, 0),
            ("Folder/remote.bin", EntryKind::File, 1, 1),
        ]);
        assert!(matches!(
            build_plan(
                &root,
                &local_inventory,
                &remote_inventory,
                &rules(&[]),
                &options(false),
            ),
            Err(Error::UnsafeRemotePath { reason, .. })
                if reason.contains("differ only by case")
        ));

        let remote_collision = remote(&[
            ("Archive", EntryKind::Directory, 0, 0),
            ("archive", EntryKind::Directory, 0, 0),
        ]);
        assert!(matches!(
            build_plan(
                &root,
                &local(&[]),
                &remote_collision,
                &rules(&[]),
                &options(false),
            ),
            Err(Error::UnsafeRemotePath { reason, .. })
                if reason.contains("differ only by case")
        ));
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

    #[test]
    fn content_mode_detects_mtime_only_changes_at_file_station_resolution() {
        let mut local = local(&[("mtime-only.bin", EntryKind::File, 4, 3_999)]);
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
