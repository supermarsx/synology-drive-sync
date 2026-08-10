use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::UNIX_EPOCH;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use md5::{Digest, Md5};

use crate::cancel::CancellationToken;
use crate::integrity::ContentMd5;
use crate::path::{drive_path_issue, is_dsm_managed, path_for_match, validate_relative};
use crate::{Error, Result};

pub const DEFAULT_IGNORE_FILE: &str = ".sdsyncignore";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LocalEntry {
    pub relative: String,
    pub full_path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: i64,
    pub content_md5: Option<ContentMd5>,
}

#[derive(Debug)]
pub struct LocalInventory {
    pub root: PathBuf,
    pub entries: BTreeMap<String, LocalEntry>,
}

impl LocalInventory {
    pub fn files(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.kind == EntryKind::File)
            .count()
    }
}

pub struct IgnoreRules {
    matcher: Gitignore,
}

impl IgnoreRules {
    pub fn build(source: &Path, extra_patterns: &[String]) -> Result<Self> {
        let source_metadata =
            fs::symlink_metadata(source).map_err(|source_error| Error::FileIo {
                path: source.to_owned(),
                source: source_error,
            })?;
        if is_link_or_reparse(&source_metadata) {
            return Err(Error::UnsupportedLocalEntry {
                path: source.to_owned(),
                reason: "the source root itself is a symbolic link, junction, or reparse point"
                    .to_owned(),
            });
        }
        let source_root = fs::canonicalize(source).map_err(|source_error| Error::FileIo {
            path: source.to_owned(),
            source: source_error,
        })?;
        reject_filesystem_root(&source_root)?;
        let mut builder = GitignoreBuilder::new(&source_root);
        let default_file = source_root.join(DEFAULT_IGNORE_FILE);
        match fs::symlink_metadata(&default_file) {
            Ok(metadata) => {
                if is_link_or_reparse(&metadata) || !metadata.is_file() {
                    return Err(Error::UnsupportedLocalEntry {
                        path: default_file,
                        reason: ".sdsyncignore must be a regular file, not a link, reparse point, or directory"
                            .to_owned(),
                    });
                }
                if let Some(error) = builder.add(&default_file) {
                    return Err(Error::Message(format!(
                        "invalid ignore file {}: {error}",
                        default_file.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::FileIo {
                    path: default_file,
                    source,
                });
            }
        }
        for pattern in extra_patterns {
            builder.add_line(None, pattern).map_err(|error| {
                Error::Message(format!("invalid --exclude pattern {pattern:?}: {error}"))
            })?;
        }
        let matcher = builder
            .build()
            .map_err(|error| Error::Message(format!("failed to build ignore rules: {error}")))?;
        Ok(Self { matcher })
    }

    pub fn is_ignored(&self, relative: &str, is_dir: bool) -> bool {
        if relative.is_empty() {
            return false;
        }
        // This file configures the sync and is not payload. Treating it as protected on
        // both sides also prevents it from defeating the empty-source deletion fuse.
        if relative == DEFAULT_IGNORE_FILE {
            return true;
        }
        self.matcher
            .matched_path_or_any_parents(path_for_match(relative), is_dir)
            .is_ignore()
    }
}

pub fn scan(source: &Path, rules: &IgnoreRules) -> Result<LocalInventory> {
    let source_metadata = fs::symlink_metadata(source).map_err(|source_error| Error::FileIo {
        path: source.to_owned(),
        source: source_error,
    })?;
    if is_link_or_reparse(&source_metadata) {
        return Err(Error::UnsupportedLocalEntry {
            path: source.to_owned(),
            reason: "the source root itself is a symbolic link or junction".to_owned(),
        });
    }
    let root = fs::canonicalize(source).map_err(|source_error| Error::FileIo {
        path: source.to_owned(),
        source: source_error,
    })?;
    if !root.is_dir() {
        return Err(Error::InvalidSource(root));
    }
    reject_filesystem_root(&root)?;
    if path_contains_dsm_managed_component(&root) {
        return Err(Error::UnsupportedLocalEntry {
            path: root,
            reason: "a DSM-managed directory cannot be selected as the source root".to_owned(),
        });
    }

    let mut entries = BTreeMap::new();
    scan_dir(&root, "", rules, &mut entries)?;
    if let Some((first, second)) = portable_case_collision(entries.keys()) {
        return Err(Error::UnsupportedLocalEntry {
            path: root.join(&second),
            reason: format!(
                "{first:?} and {second:?} differ only by case and cannot both sync to Windows Drive clients"
            ),
        });
    }
    Ok(LocalInventory { root, entries })
}

pub fn populate_content_md5(
    inventory: &mut LocalInventory,
    cancellation: &CancellationToken,
) -> Result<()> {
    for entry in inventory.entries.values_mut() {
        cancellation.check()?;
        if entry.kind == EntryKind::File {
            entry.content_md5 = Some(hash_file_snapshot(entry, cancellation)?);
        }
    }
    Ok(())
}

pub fn hash_file_snapshot(
    entry: &LocalEntry,
    cancellation: &CancellationToken,
) -> Result<ContentMd5> {
    verify_entry_snapshot(entry)?;
    let mut file = fs::File::open(&entry.full_path).map_err(|source| Error::FileIo {
        path: entry.full_path.clone(),
        source,
    })?;
    verify_open_snapshot(entry, &file)?;

    let mut hasher = Md5::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        cancellation.check()?;
        let count = file.read(&mut buffer).map_err(|source| Error::FileIo {
            path: entry.full_path.clone(),
            source,
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    verify_open_snapshot(entry, &file)?;
    verify_entry_snapshot(entry)?;
    Ok(ContentMd5::from_bytes(hasher.finalize().into()))
}

fn verify_entry_snapshot(entry: &LocalEntry) -> Result<()> {
    let metadata = fs::symlink_metadata(&entry.full_path).map_err(|source| Error::FileIo {
        path: entry.full_path.clone(),
        source,
    })?;
    if is_link_or_reparse(&metadata) {
        return Err(Error::SourceChanged(entry.full_path.clone()));
    }
    verify_metadata_snapshot(entry, &metadata)
}

fn verify_open_snapshot(entry: &LocalEntry, file: &fs::File) -> Result<()> {
    let metadata = file.metadata().map_err(|source| Error::FileIo {
        path: entry.full_path.clone(),
        source,
    })?;
    verify_metadata_snapshot(entry, &metadata)
}

fn verify_metadata_snapshot(entry: &LocalEntry, metadata: &fs::Metadata) -> Result<()> {
    let modified = metadata.modified().map_err(|source| Error::FileIo {
        path: entry.full_path.clone(),
        source,
    })?;
    let millis = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    if !metadata.is_file() || metadata.len() != entry.size || millis != Some(entry.mtime_ms) {
        return Err(Error::SourceChanged(entry.full_path.clone()));
    }
    Ok(())
}

fn scan_dir(
    directory: &Path,
    relative_parent: &str,
    rules: &IgnoreRules,
    output: &mut BTreeMap<String, LocalEntry>,
) -> Result<()> {
    let reader = fs::read_dir(directory).map_err(|source| Error::FileIo {
        path: directory.to_owned(),
        source,
    })?;
    let mut children = Vec::new();
    for child in reader {
        let child = child.map_err(|source| Error::FileIo {
            path: directory.to_owned(),
            source,
        })?;
        children.push(child);
    }
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let name = child
            .file_name()
            .into_string()
            .map_err(|_| Error::UnsupportedLocalEntry {
                path: child.path(),
                reason: "name is not valid UTF-8".to_owned(),
            })?;
        let relative = if relative_parent.is_empty() {
            name
        } else {
            format!("{relative_parent}/{name}")
        };
        // DSM creates these administrative entries inside otherwise ordinary shares. They are
        // never payload, and pruning them before metadata lookup guarantees that a linked or
        // otherwise unusual managed entry is not traversed. Remote planning protects the same
        // names, so mirror mode cannot interpret the omission as authorization to delete them.
        if is_dsm_managed(&relative) {
            continue;
        }

        validate_relative(&relative)?;
        if let Some(reason) = drive_path_issue(&relative) {
            return Err(Error::UnsupportedLocalEntry {
                path: child.path(),
                reason,
            });
        }

        let file_type = child.file_type().map_err(|source| Error::FileIo {
            path: child.path(),
            source,
        })?;
        if rules.is_ignored(&relative, file_type.is_dir()) {
            continue;
        }
        let metadata = fs::symlink_metadata(child.path()).map_err(|source| Error::FileIo {
            path: child.path(),
            source,
        })?;
        if is_link_or_reparse(&metadata) {
            return Err(Error::UnsupportedLocalEntry {
                path: child.path(),
                reason: "symbolic links, junctions, and reparse points are not followed".to_owned(),
            });
        }
        if let Some(reason) = unsupported_windows_attributes(&metadata) {
            return Err(Error::UnsupportedLocalEntry {
                path: child.path(),
                reason: reason.to_owned(),
            });
        }

        if metadata.is_dir() {
            let full_path = child.path();
            output.insert(
                relative.clone(),
                LocalEntry {
                    relative: relative.clone(),
                    full_path: full_path.clone(),
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_ms: 0,
                    content_md5: None,
                },
            );
            scan_dir(&full_path, &relative, rules, output)?;
        } else if metadata.is_file() {
            let full_path = child.path();
            let modified = metadata.modified().map_err(|source| Error::FileIo {
                path: full_path.clone(),
                source,
            })?;
            let duration =
                modified
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| Error::UnsupportedLocalEntry {
                        path: full_path.clone(),
                        reason: "modification time is before the Unix epoch".to_owned(),
                    })?;
            let mtime_ms =
                i64::try_from(duration.as_millis()).map_err(|_| Error::UnsupportedLocalEntry {
                    path: full_path.clone(),
                    reason: "modification time is outside DSM's supported range".to_owned(),
                })?;
            output.insert(
                relative.clone(),
                LocalEntry {
                    relative,
                    full_path,
                    kind: EntryKind::File,
                    size: metadata.len(),
                    mtime_ms,
                    content_md5: None,
                },
            );
        } else {
            return Err(Error::UnsupportedLocalEntry {
                path: child.path(),
                reason: "only regular files and directories can be synchronized".to_owned(),
            });
        }
    }
    Ok(())
}

fn path_contains_dsm_managed_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name.to_str().is_some_and(is_dsm_managed),
        _ => false,
    })
}

fn reject_filesystem_root(path: &Path) -> Result<()> {
    if path.parent().is_none() && !is_unc_share_root(path) {
        return Err(Error::UnsupportedLocalEntry {
            path: path.to_owned(),
            reason: "the canonical source root cannot be a filesystem root".to_owned(),
        });
    }
    Ok(())
}

// An SMB share root has no parent, exactly like `/` or `C:\`, but a share is the network
// analogue of a folder rather than of a disk: `\\nas\media` is a perfectly ordinary thing to
// ask to sync, and refusing it while accepting `\\nas\media\photos` is arbitrary. A mapped
// drive letter is a disk-like alias, so `\\?\Z:\` stays rejected along with the real roots.
// A prefix that names a server but no share (`\\?\UNC\nas`) is not a directory anyone can
// sync either, so an empty share is rejected too.
fn is_unc_share_root(path: &Path) -> bool {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    match prefix.kind() {
        Prefix::UNC(_, share) | Prefix::VerbatimUNC(_, share) => !share.is_empty(),
        _ => false,
    }
}

fn portable_case_collision<'a>(
    paths: impl IntoIterator<Item = &'a String>,
) -> Option<(String, String)> {
    let mut seen = BTreeMap::new();
    for path in paths {
        let folded = path.to_lowercase();
        if let Some(first) = seen.insert(folded, path.clone())
            && first != *path
        {
            return Some((first, path.clone()));
        }
    }
    None
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn unsupported_windows_attributes(metadata: &fs::Metadata) -> Option<&'static str> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0004;
    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x0100;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    let attributes = metadata.file_attributes();
    if attributes & (FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_TEMPORARY | FILE_ATTRIBUTE_OFFLINE) != 0
    {
        Some("Windows OFFLINE, SYSTEM, and TEMPORARY entries are unsupported by Synology Drive")
    } else {
        None
    }
}

#[cfg(not(windows))]
fn unsupported_windows_attributes(_metadata: &fs::Metadata) -> Option<&'static str> {
    None
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("sdsync-{name}-{}-{nonce}", std::process::id(),));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    // Directory junctions (Windows) and symlinks (Unix) are the two portable ways to build a
    // reparse point without elevated privileges or Developer Mode; Windows symlinks require
    // both and would make CI flaky on unprivileged runners, so junctions stand in for them here.
    #[cfg(windows)]
    fn try_make_link(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    fn try_make_link(link: &Path, target: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[test]
    fn scan_is_deterministic_and_honors_rules() {
        let root = temp_dir("scan");
        fs::create_dir(root.join("keep")).unwrap();
        fs::write(root.join("keep/b.txt"), b"b").unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("skip.tmp"), b"x").unwrap();
        let mut ignore = fs::File::create(root.join(DEFAULT_IGNORE_FILE)).unwrap();
        writeln!(ignore, "*.tmp").unwrap();

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules).unwrap();
        let names: Vec<_> = inventory.entries.keys().cloned().collect();
        assert_eq!(names, ["a.txt", "keep", "keep/b.txt"]);
        assert_eq!(inventory.files(), 2);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prunes_dsm_managed_entries_without_losing_user_directories() {
        let root = temp_dir("managed-prune");
        fs::write(root.join("payload.txt"), b"payload").unwrap();
        fs::create_dir_all(root.join("album/@eaDir/thumbnails")).unwrap();
        fs::write(root.join("album/@eaDir/thumbnails/preview.jpg"), b"preview").unwrap();
        fs::create_dir(root.join("#recycle")).unwrap();
        fs::write(root.join("#recycle/deleted.txt"), b"deleted").unwrap();
        fs::create_dir(root.join("#snapshot")).unwrap();
        fs::write(root.join("#snapshot/history.txt"), b"history").unwrap();
        fs::create_dir(root.join("@appdata")).unwrap();
        fs::write(root.join("@appdata/package.db"), b"private package data").unwrap();
        fs::write(root.join("@tmp"), b"administrative placeholder").unwrap();

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules).unwrap();
        assert_eq!(
            inventory.entries.keys().cloned().collect::<Vec<_>>(),
            ["album", "payload.txt"]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_a_source_root_inside_a_dsm_managed_directory() {
        let parent = temp_dir("managed-root");
        let root = parent.join("@eaDir").join("nested");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("payload.txt"), b"payload").unwrap();

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&root, &rules),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == fs::canonicalize(&root).unwrap()
                    && reason.contains("source root")
        ));

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn rejects_canonical_filesystem_roots_before_reading_or_scanning_them() {
        let fixture = temp_dir("filesystem-root");
        let canonical_fixture = fs::canonicalize(&fixture).unwrap();
        let filesystem_root = canonical_fixture
            .ancestors()
            .find(|ancestor| ancestor.parent().is_none())
            .unwrap()
            .to_owned();
        let rules = IgnoreRules::build(&fixture, &[]).unwrap();

        assert!(matches!(
            IgnoreRules::build(&filesystem_root, &[]),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == filesystem_root && reason.contains("filesystem root")
        ));
        assert!(matches!(
            scan(&filesystem_root, &rules),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == filesystem_root && reason.contains("filesystem root")
        ));

        fs::remove_dir_all(fixture).unwrap();
    }

    #[track_caller]
    fn assert_rejected_as_filesystem_root(root: &str) {
        let root = Path::new(root);
        assert!(
            matches!(
                reject_filesystem_root(root),
                Err(Error::UnsupportedLocalEntry { path, reason })
                    if path == root && reason.contains("filesystem root")
            ),
            "{} should be rejected as a filesystem root",
            root.display()
        );
    }

    #[test]
    fn rejects_the_unix_filesystem_root_by_name() {
        assert_rejected_as_filesystem_root("/");
    }

    // An SMB share is a folder-like unit, so a share root is a legitimate source even though
    // it has no parent. Drive-shaped roots, including a mapped network drive, stay rejected.
    #[cfg(windows)]
    #[test]
    fn accepts_smb_share_roots_but_still_rejects_drive_and_server_roots() {
        // Canonicalizing a real share root yields the verbatim form; the plain form is what a
        // user types. `is_dir` holds for both, so only this guard decides the outcome.
        for accepted in [
            r"\\?\UNC\server\share",
            r"\\?\UNC\localhost\C$",
            r"\\server\share",
        ] {
            assert!(
                reject_filesystem_root(Path::new(accepted)).is_ok(),
                "{accepted} should be accepted as a share root"
            );
        }

        // A drive letter names a disk, not a folder, whether or not it is a mapped network
        // drive, and a UNC prefix naming no share is not a directory at all.
        for rejected in [r"\\?\C:\", r"C:\", r"\\?\Z:\", r"Z:\", r"\\?\UNC\server"] {
            assert_rejected_as_filesystem_root(rejected);
        }
    }

    // A share root reaches the guard only after canonicalization, so prove the accepted form
    // is exactly what `fs::canonicalize` produces rather than a hand-written approximation.
    #[cfg(windows)]
    #[test]
    fn treats_a_canonicalized_share_root_as_a_sync_candidate() {
        let Ok(share_root) = fs::canonicalize(r"\\localhost\C$") else {
            eprintln!(
                "skipping treats_a_canonicalized_share_root_as_a_sync_candidate: no admin share"
            );
            return;
        };
        assert_eq!(share_root.parent(), None);
        assert!(reject_filesystem_root(&share_root).is_ok());
    }

    #[test]
    fn rejects_a_link_as_the_source_root() {
        let parent = temp_dir("link-root");
        let target = parent.join("target");
        fs::create_dir(&target).unwrap();
        let link = parent.join("link");
        if !try_make_link(&link, &target) {
            eprintln!("skipping rejects_a_link_as_the_source_root: could not create a link");
            fs::remove_dir_all(parent).unwrap();
            return;
        }

        assert!(matches!(
            IgnoreRules::build(&link, &[]),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == link && reason.contains("symbolic link")
        ));
        let rules = IgnoreRules::build(&target, &[]).unwrap();
        assert!(matches!(
            scan(&link, &rules),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == link && reason.contains("symbolic link")
        ));

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn scan_rejects_a_link_within_the_source_tree() {
        let root = temp_dir("link-child");
        let outside = temp_dir("link-child-target");
        fs::write(outside.join("payload.txt"), b"payload").unwrap();
        let link = root.join("linked");
        if !try_make_link(&link, &outside) {
            eprintln!(
                "skipping scan_rejects_a_link_within_the_source_tree: could not create a link"
            );
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(outside).unwrap();
            return;
        }
        let expected_link_path = fs::canonicalize(&root).unwrap().join("linked");

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&root, &rules),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == expected_link_path && reason.contains("not followed")
        ));

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn scan_rejects_case_colliding_entries_on_case_sensitive_filesystems() {
        let root = temp_dir("case-collision-scan");
        fs::write(root.join("Config.txt"), b"a").unwrap();
        fs::write(root.join("config.txt"), b"b").unwrap();
        if fs::read_dir(&root).unwrap().count() < 2 {
            // The filesystem folded the two names together (the default on Windows and
            // macOS), so there is nothing left to collide. Skip on this platform.
            fs::remove_dir_all(root).unwrap();
            return;
        }
        let expected_path = fs::canonicalize(&root).unwrap().join("config.txt");

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&root, &rules),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == expected_path && reason.contains("differ only by case")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_nonportable_drive_names_before_upload() {
        for relative in ["~scratch", "bad:name", "folder/NUL.txt", "trailing."] {
            assert!(drive_path_issue(relative).is_some(), "{relative}");
        }
        assert!(drive_path_issue("normal/fichier.txt").is_none());
    }

    #[test]
    fn detects_portability_case_collisions() {
        let paths = ["Folder/A.txt".to_owned(), "folder/a.TXT".to_owned()];
        assert!(portable_case_collision(paths.iter()).is_some());
        let distinct = ["Folder/A.txt".to_owned(), "Folder/B.txt".to_owned()];
        assert!(portable_case_collision(distinct.iter()).is_none());
    }

    #[test]
    fn content_hashing_records_the_scanned_file_snapshot() {
        let root = temp_dir("content-md5");
        fs::write(root.join("payload.bin"), b"abc").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let mut inventory = scan(&root, &rules).unwrap();

        populate_content_md5(&mut inventory, &CancellationToken::default()).unwrap();
        assert_eq!(
            inventory.entries["payload.bin"].content_md5,
            Some(ContentMd5::parse_hex("900150983cd24fb0d6963f7d28e17f72").unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignore_control_path_must_be_a_regular_file() {
        let root = temp_dir("ignore-control-directory");
        let control_path = root.join(DEFAULT_IGNORE_FILE);
        fs::create_dir(&control_path).unwrap();
        let expected_control_path = fs::canonicalize(&control_path).unwrap();

        let result = IgnoreRules::build(&root, &[]);
        assert!(matches!(
            result,
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == expected_control_path && reason.contains("regular file")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hashing_rejects_a_file_changed_since_the_scan() {
        let root = temp_dir("changed-before-hash");
        let payload = root.join("payload.bin");
        fs::write(&payload, b"abc").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules).unwrap();
        let scanned = inventory.entries["payload.bin"].clone();
        let scanned_path = scanned.full_path.clone();

        fs::write(&payload, b"content changed after the scan").unwrap();

        assert!(matches!(
            hash_file_snapshot(&scanned, &CancellationToken::default()),
            Err(Error::SourceChanged(path)) if path == scanned_path
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_cancelled_content_hashing_returns_no_partial_digests() {
        let root = temp_dir("cancelled-content-md5");
        fs::write(root.join("a.bin"), b"a").unwrap();
        fs::write(root.join("b.bin"), b"b").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let mut inventory = scan(&root, &rules).unwrap();
        let cancellation = CancellationToken::default();
        cancellation.cancel();

        assert!(matches!(
            populate_content_md5(&mut inventory, &cancellation),
            Err(Error::Cancelled)
        ));
        assert!(
            inventory
                .entries
                .values()
                .all(|entry| entry.content_md5.is_none())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_rejects_a_regular_file_as_the_source_root() {
        let root = temp_dir("source-file");
        let source_file = root.join("payload.bin");
        fs::write(&source_file, b"payload").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let expected = fs::canonicalize(&source_file).unwrap();

        assert!(matches!(
            scan(&source_file, &rules),
            Err(Error::InvalidSource(path)) if path == expected
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_sources_and_removed_hash_inputs_report_the_exact_path() {
        let root = temp_dir("missing-source");
        let missing = root.join("does-not-exist");
        assert!(matches!(
            IgnoreRules::build(&missing, &[]),
            Err(Error::FileIo { path, .. }) if path == missing
        ));
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&missing, &rules),
            Err(Error::FileIo { path, .. }) if path == missing
        ));

        let payload = root.join("payload.bin");
        fs::write(&payload, b"payload").unwrap();
        let entry = scan(&root, &rules).unwrap().entries["payload.bin"].clone();
        let expected_entry_path = entry.full_path.clone();
        fs::remove_file(&payload).unwrap();
        assert!(matches!(
            hash_file_snapshot(&entry, &CancellationToken::default()),
            Err(Error::FileIo { path, .. }) if path == expected_entry_path
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignore_rules_protect_the_control_file_and_apply_parent_patterns() {
        let root = temp_dir("ignore-semantics");
        let rules = IgnoreRules::build(&root, &["cache/".to_owned(), "*.tmp".to_owned()]).unwrap();
        assert!(!rules.is_ignored("", true));
        assert!(rules.is_ignored(DEFAULT_IGNORE_FILE, false));
        assert!(rules.is_ignored("cache", true));
        assert!(rules.is_ignored("cache/nested/file.bin", false));
        assert!(rules.is_ignored("folder/scratch.tmp", false));
        assert!(!rules.is_ignored("folder/keep.txt", false));
        assert_eq!(EntryKind::File.as_str(), "file");
        assert_eq!(EntryKind::Directory.as_str(), "directory");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exclude_negation_reincludes_a_narrower_pattern() {
        // `IgnoreRules::build` feeds every `--exclude` value to `GitignoreBuilder::add_line`,
        // which honors gitignore `!` negation. This pins that a leading `!` re-includes a
        // narrower match from an earlier, broader exclusion (documented on --exclude).
        let root = temp_dir("exclude-negation");
        let rules = IgnoreRules::build(&root, &["*".to_owned(), "!*.pdf".to_owned()]).unwrap();
        assert!(rules.is_ignored("notes.txt", false));
        assert!(!rules.is_ignored("report.pdf", false));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_ignore_file_and_cli_globs_are_rejected_with_source_context() {
        let root = temp_dir("invalid-ignore");
        let invalid_pattern = "[z-a]";
        assert!(IgnoreRules::build(&root, &[invalid_pattern.to_owned()]).is_err());
        fs::write(root.join(DEFAULT_IGNORE_FILE), invalid_pattern).unwrap();
        let error = match IgnoreRules::build(&root, &[]) {
            Err(error) => error,
            Ok(_) => panic!("invalid ignore file was accepted"),
        };
        assert!(error.to_string().contains("invalid ignore file"));
        assert!(error.to_string().contains(DEFAULT_IGNORE_FILE));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drive_name_diagnostics_cover_every_portability_class() {
        let long_path = "x".repeat(248);
        for (relative, expected) in [
            (long_path.as_str(), "247-character"),
            ("~temporary", "beginning with ~"),
            ("folder/control\u{7}", "control character"),
            ("folder/bad?.txt", "unsupported by Windows"),
            ("folder/trailing ", "dot or space"),
            ("folder/COM9.log", "reserved by Windows"),
        ] {
            assert!(drive_path_issue(relative).unwrap().contains(expected));
        }
    }

    #[cfg(windows)]
    fn write_non_utf8_named_file(dir: &Path) -> bool {
        use std::os::windows::ffi::OsStringExt;

        // NTFS filenames are validated as UTF-16 code units, not as well-formed Unicode, so a
        // lone surrogate is a legal (if unusual) file name that OsString::into_string cannot
        // represent as valid Unicode.
        let wide: Vec<u16> = vec![u16::from(b'b'), 0xD800, u16::from(b'b')];
        let name = std::ffi::OsString::from_wide(&wide);
        fs::write(dir.join(&name), b"payload").is_ok()
    }

    #[cfg(unix)]
    fn write_non_utf8_named_file(dir: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;

        let bytes = [b'b', 0xFF, b'b'];
        let name = std::ffi::OsStr::from_bytes(&bytes);
        fs::write(dir.join(name), b"payload").is_ok()
    }

    #[test]
    fn scan_rejects_a_non_utf8_file_name() {
        let root = temp_dir("non-utf8-name");
        if !write_non_utf8_named_file(&root) {
            eprintln!("skipping scan_rejects_a_non_utf8_file_name: filesystem rejected the name");
            fs::remove_dir_all(root).unwrap();
            return;
        }

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&root, &rules),
            Err(Error::UnsupportedLocalEntry { reason, .. }) if reason.contains("UTF-8")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_dir_reports_the_missing_directory_it_could_not_read() {
        let root = temp_dir("scan-dir-missing");
        let missing = root.join("does-not-exist");
        let mut output = BTreeMap::new();
        let rules = IgnoreRules::build(&root, &[]).unwrap();

        assert!(matches!(
            scan_dir(&missing, "does-not-exist", &rules, &mut output),
            Err(Error::FileIo { path, .. }) if path == missing
        ));
        assert!(output.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hashing_rejects_a_file_replaced_by_a_link_since_the_scan() {
        let root = temp_dir("replaced-by-link");
        let target_dir = temp_dir("replaced-by-link-target");
        let replaced = root.join("replaced.bin");
        fs::write(&replaced, b"payload").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let entry = scan(&root, &rules).unwrap().entries["replaced.bin"].clone();

        fs::remove_file(&replaced).unwrap();
        if !try_make_link(&replaced, &target_dir) {
            eprintln!(
                "skipping hashing_rejects_a_file_replaced_by_a_link_since_the_scan: could not create a link"
            );
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(target_dir).unwrap();
            return;
        }

        assert!(matches!(
            hash_file_snapshot(&entry, &CancellationToken::default()),
            Err(Error::SourceChanged(path)) if path == entry.full_path
        ));

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&target_dir).unwrap();
    }

    #[test]
    fn scan_rejects_files_modified_before_the_unix_epoch() {
        let root = temp_dir("before-epoch");
        let target = root.join("old.bin");
        fs::write(&target, b"payload").unwrap();

        let before_epoch = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        let file = fs::OpenOptions::new().write(true).open(&target).unwrap();
        let set = file.set_times(fs::FileTimes::new().set_modified(before_epoch));
        drop(file);
        if set.is_err() {
            eprintln!(
                "skipping scan_rejects_files_modified_before_the_unix_epoch: platform or \
                 filesystem rejected a pre-epoch mtime"
            );
            fs::remove_dir_all(root).unwrap();
            return;
        }

        let expected_path = fs::canonicalize(&target).unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&root, &rules),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == expected_path && reason.contains("before the Unix epoch")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn scan_rejects_windows_system_attribute_files() {
        let root = temp_dir("windows-system-attribute");
        let target = root.join("system.dat");
        fs::write(&target, b"payload").unwrap();

        let status = std::process::Command::new("attrib")
            .arg("+S")
            .arg(&target)
            .status();
        if !matches!(status, Ok(status) if status.success()) {
            eprintln!(
                "skipping scan_rejects_windows_system_attribute_files: could not set the SYSTEM attribute"
            );
            fs::remove_dir_all(root).unwrap();
            return;
        }

        let expected_path = fs::canonicalize(&target).unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&root, &rules),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == expected_path && reason.contains("SYSTEM")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn hashing_reports_the_open_failure_when_another_handle_holds_an_exclusive_lock() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = temp_dir("exclusive-lock");
        let target = root.join("locked.bin");
        fs::write(&target, b"payload").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let entry = scan(&root, &rules).unwrap().entries["locked.bin"].clone();

        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target);
        let lock = match lock {
            Ok(lock) => lock,
            Err(_) => {
                eprintln!(
                    "skipping hashing_reports_the_open_failure_when_another_handle_holds_an_exclusive_lock: \
                     could not take an exclusive lock"
                );
                fs::remove_dir_all(root).unwrap();
                return;
            }
        };

        let result = hash_file_snapshot(&entry, &CancellationToken::default());
        drop(lock);
        assert!(matches!(
            result,
            Err(Error::FileIo { path, .. }) if path == entry.full_path
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hashing_reports_the_open_failure_when_read_permission_is_denied() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("permission-denied-open");
        let target = root.join("secret.bin");
        fs::write(&target, b"payload").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let entry = scan(&root, &rules).unwrap().entries["secret.bin"].clone();

        fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
        let result = hash_file_snapshot(&entry, &CancellationToken::default());
        // Restore permissions unconditionally: cleanup needs it, and a root-owned CI runner
        // that ignores mode bits must not leave an unreadable fixture behind either way.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

        if result.is_ok() {
            eprintln!(
                "skipping hashing_reports_the_open_failure_when_read_permission_is_denied: \
                 running with privileges that bypass file permissions"
            );
            fs::remove_dir_all(root).unwrap();
            return;
        }
        assert!(matches!(
            result,
            Err(Error::FileIo { path, .. }) if path == entry.full_path
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_rejects_unusual_file_types() {
        let root = temp_dir("fifo-entry");
        let fifo_path = root.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status();
        if !matches!(status, Ok(status) if status.success()) {
            eprintln!("skipping scan_rejects_unusual_file_types: mkfifo unavailable");
            fs::remove_dir_all(root).unwrap();
            return;
        }

        let expected_path = fs::canonicalize(&fifo_path).unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&root, &rules),
            Err(Error::UnsupportedLocalEntry { path, reason })
                if path == expected_path && reason.contains("only regular files and directories")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    // If the source itself is a regular file, ".sdsyncignore" becomes "<file>/.sdsyncignore" -
    // stating a path that walks through a file as if it were a directory. POSIX reports that as
    // ENOTDIR, a real (non-NotFound) I/O error, which pins the `Err(source) if not NotFound`
    // branch of IgnoreRules::build distinctly from the ordinary "no ignore file present" case
    // handled just above it. Windows instead reports plain NotFound for the same composition
    // (verified empirically: denying directory permissions via icacls also would not reliably
    // reproduce a non-NotFound stat error on this platform), so there is no portable trigger and
    // this stays POSIX-only.
    #[cfg(unix)]
    #[test]
    fn ignore_rules_report_a_non_not_found_stat_error_on_the_default_ignore_path() {
        let root = temp_dir("ignore-control-not-a-directory");
        let source_file = root.join("payload.bin");
        fs::write(&source_file, b"payload").unwrap();
        let expected_default_file = fs::canonicalize(&source_file)
            .unwrap()
            .join(DEFAULT_IGNORE_FILE);

        let error = match IgnoreRules::build(&source_file, &[]) {
            Err(error) => error,
            Ok(_) => panic!("a file used as the source root was accepted"),
        };
        assert!(matches!(
            error,
            Error::FileIo { path, source } if path == expected_default_file
                && source.kind() != std::io::ErrorKind::NotFound
        ));

        fs::remove_dir_all(root).unwrap();
    }
}
