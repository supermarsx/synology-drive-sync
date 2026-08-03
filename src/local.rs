use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::path::{is_dsm_managed, path_for_match, validate_relative};
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
        validate_relative(&relative)?;
        if let Some(reason) = drive_name_issue(&relative) {
            return Err(Error::UnsupportedLocalEntry {
                path: child.path(),
                reason,
            });
        }

        if is_dsm_managed(&relative) {
            return Err(Error::UnsupportedLocalEntry {
                path: child.path(),
                reason: "DSM-managed names (#recycle, #snapshot, and @eaDir) are not sync payload"
                    .to_owned(),
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

fn drive_name_issue(relative: &str) -> Option<String> {
    if relative.chars().count() > 247 {
        return Some(
            "relative path exceeds Synology Drive's 247-character Windows compatibility limit"
                .to_owned(),
        );
    }
    for component in relative.split('/') {
        if component.chars().count() > 255 {
            return Some(
                "a path component exceeds Synology Drive's 255-character limit".to_owned(),
            );
        }
        if component.starts_with('~') {
            return Some(
                "names beginning with ~ are not synchronized by Synology Drive".to_owned(),
            );
        }
        if component.chars().any(char::is_control) {
            return Some("name contains a terminal-unsafe control character".to_owned());
        }
        if component.contains(['*', ':', '?', '"', '<', '>', '|']) {
            return Some(
                "name contains characters unsupported by Windows Synology Drive clients".to_owned(),
            );
        }
        if component.ends_with(['.', ' ']) {
            return Some("name ends with a dot or space and is not Windows-compatible".to_owned());
        }

        let stem = component.split('.').next().unwrap_or(component);
        if matches!(
            stem.to_ascii_uppercase().as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        ) {
            return Some("name is reserved by Windows and cannot sync portably".to_owned());
        }
    }
    None
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
    fn rejects_dsm_managed_local_names() {
        let root = temp_dir("managed");
        fs::create_dir(root.join("@eaDir")).unwrap();

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        assert!(matches!(
            scan(&root, &rules),
            Err(Error::UnsupportedLocalEntry { .. })
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_nonportable_drive_names_before_upload() {
        for relative in ["~scratch", "bad:name", "folder/NUL.txt", "trailing."] {
            assert!(drive_name_issue(relative).is_some(), "{relative}");
        }
        assert!(drive_name_issue("normal/fichier.txt").is_none());
    }

    #[test]
    fn detects_portability_case_collisions() {
        let paths = ["Folder/A.txt".to_owned(), "folder/a.TXT".to_owned()];
        assert!(portable_case_collision(paths.iter()).is_some());
        let distinct = ["Folder/A.txt".to_owned(), "Folder/B.txt".to_owned()];
        assert!(portable_case_collision(distinct.iter()).is_none());
    }
}
