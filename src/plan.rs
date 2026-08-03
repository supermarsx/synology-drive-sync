use std::collections::{BTreeSet, HashSet};

use crate::api::{RemoteEntry, RemoteInventory};
use crate::local::{EntryKind, IgnoreRules, LocalEntry, LocalInventory};
use crate::path::{RemoteRoot, depth, is_dsm_managed};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareMode {
    Metadata,
    SizeOnly,
}

#[derive(Clone, Debug)]
pub struct PlanOptions {
    pub delete: bool,
    pub allow_empty_source: bool,
    pub max_delete: usize,
    pub compare: CompareMode,
}

#[derive(Clone, Debug)]
pub struct CreateAction {
    pub relative: String,
    pub remote_path: String,
}

#[derive(Clone, Debug)]
pub struct UploadAction {
    pub local: LocalEntry,
    pub remote_path: String,
}

#[derive(Clone, Debug)]
pub struct DeleteAction {
    pub relative: String,
    pub remote_path: String,
    pub kind: EntryKind,
    pub type_conflict: bool,
}

#[derive(Debug)]
pub struct SyncPlan {
    pub pre_deletes: Vec<DeleteAction>,
    pub creates: Vec<CreateAction>,
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
            && self.uploads.is_empty()
            && self.post_deletes.is_empty()
    }
}

pub fn build_plan(
    root: &RemoteRoot,
    local: &LocalInventory,
    remote: &RemoteInventory,
    rules: &IgnoreRules,
    options: &PlanOptions,
) -> Result<SyncPlan> {
    let mut plan = SyncPlan {
        pre_deletes: Vec::new(),
        creates: Vec::new(),
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
                if files_match(entry, remote_entry, options.compare) {
                    plan.unchanged_files += 1;
                } else {
                    schedule_upload(root, &mut plan, entry)?;
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
                push_predelete(&mut plan, remote_entry, &mut predeleted);
                schedule_create(root, &mut plan, entry)?;
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
                    push_predelete(&mut plan, candidate, &mut predeleted);
                }
                schedule_upload(root, &mut plan, entry)?;
            }
        }
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
            plan.post_deletes.push(DeleteAction {
                relative: entry.relative.clone(),
                remote_path: entry.remote_path.clone(),
                kind: entry.kind,
                type_conflict: false,
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

fn schedule_missing(root: &RemoteRoot, plan: &mut SyncPlan, entry: &LocalEntry) -> Result<()> {
    match entry.kind {
        EntryKind::Directory => schedule_create(root, plan, entry),
        EntryKind::File => schedule_upload(root, plan, entry),
    }
}

fn schedule_create(root: &RemoteRoot, plan: &mut SyncPlan, entry: &LocalEntry) -> Result<()> {
    plan.creates.push(CreateAction {
        relative: entry.relative.clone(),
        remote_path: root.join(&entry.relative)?,
    });
    Ok(())
}

fn schedule_upload(root: &RemoteRoot, plan: &mut SyncPlan, entry: &LocalEntry) -> Result<()> {
    plan.upload_bytes = plan.upload_bytes.saturating_add(entry.size);
    plan.uploads.push(UploadAction {
        local: entry.clone(),
        remote_path: root.join(&entry.relative)?,
    });
    Ok(())
}

fn push_predelete(plan: &mut SyncPlan, remote: &RemoteEntry, predeleted: &mut BTreeSet<String>) {
    if predeleted.insert(remote.relative.clone()) {
        plan.pre_deletes.push(DeleteAction {
            relative: remote.relative.clone(),
            remote_path: remote.remote_path.clone(),
            kind: remote.kind,
            type_conflict: true,
        });
    }
}

fn files_match(local: &LocalEntry, remote: &RemoteEntry, mode: CompareMode) -> bool {
    if local.size != remote.size {
        return false;
    }
    match mode {
        CompareMode::SizeOnly => true,
        CompareMode::Metadata => local.mtime_ms.div_euclid(1000) == remote.mtime_seconds,
    }
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
}
