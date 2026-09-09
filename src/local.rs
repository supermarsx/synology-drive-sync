use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::UNIX_EPOCH;

use crate::cancel::CancellationToken;
use crate::integrity::{ContentHasher, ContentMd5, Md5ContentHasher};
use crate::path::{drive_path_issue, is_dsm_managed, path_for_match, validate_relative};
use crate::plan::Scope;
use crate::{Error, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

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

/// Ceiling on entries recorded by one scoped scan, per side.
///
/// The page cap bounds a status *response*; it does not bound the walk that produces one. A scoped
/// scan materializes the local and remote inventories in full before anything is compared, so on
/// the armv7 target the binding constraint is memory rather than time, and this is where that
/// ceiling is enforced.
///
/// Derived, not chosen. `tests/scan_memory.rs` measures the real heap cost of one scanned entry on
/// both sides with a counting allocator and holds the two inventories for one request under a
/// 32 MiB ceiling:
///
/// ```text
/// measured   1,336 B/pair on Linux glibc, 1,350 B/pair on Windows (deep 99-byte paths)
/// ceiling   32 MiB = 33,554,432 bytes
/// implied   33,554,432 / 1,422 = 23,595 entries per side
/// shipped   20,000, keeping headroom for measurement spread across platforms
/// ```
///
/// That test fails if this constant is raised without re-running the measurement, so the number
/// and the evidence behind it cannot drift apart. Re-derive it there rather than re-guessing here;
/// its module comment breaks the per-entry cost down by field.
///
/// Two caveats for whoever retunes this. Roughly half of each entry is the path stored again --
/// as a map key, and as a derived absolute path on each side -- so a compact representation would
/// buy more than a larger ceiling would. And cost tracks path length, so an entry is not a fixed
/// amount of memory: shallow trees cost about half what deep ones do, and this budget is set from
/// the deep-path worst case.
///
/// A batch `sync` deliberately keeps its unbudgeted full-tree scan: it is not interactive
/// per-request work and does not share this cost budget.
pub const SCAN_BUDGET_DEFAULT: usize = 20_000;

/// The result of a scoped walk.
#[derive(Debug)]
pub struct ScopedScan {
    pub inventory: LocalInventory,
    /// Paths pruned by an ignore rule or as DSM-managed. Populated only on request.
    pub excluded: BTreeSet<String>,
    /// False when the budget stopped the walk, making the inventory a subset of the scope.
    pub complete: bool,
}

struct ScanContext<'a> {
    rules: &'a IgnoreRules,
    scope: &'a Scope,
    include_excluded: bool,
    budget: usize,
    cancellation: &'a CancellationToken,
}

struct ScanOutput {
    entries: BTreeMap<String, LocalEntry>,
    excluded: BTreeSet<String>,
    complete: bool,
}

impl Default for ScanOutput {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            excluded: BTreeSet::new(),
            complete: true,
        }
    }
}

impl ScanOutput {
    /// Record an entry, reporting whether the walk may continue.
    fn insert(&mut self, relative: String, entry: LocalEntry, budget: usize) -> bool {
        self.entries.insert(relative, entry);
        if self.entries.len() >= budget {
            self.complete = false;
            return false;
        }
        true
    }
}

/// Walk `source` into a deterministic inventory.
///
/// A large tree is one of the longest uninterruptible phases a run has, so `cancellation` is
/// consulted before every directory is opened and before every child entry is inspected. A
/// cancelled scan returns [`Error::Cancelled`] and no partial inventory.
pub fn scan(
    source: &Path,
    rules: &IgnoreRules,
    cancellation: &CancellationToken,
) -> Result<LocalInventory> {
    let scope = Scope::root();
    Ok(scan_scoped(source, rules, &scope, false, usize::MAX, cancellation)?.inventory)
}

/// Walk only the part of `source` that `scope` names.
///
/// Entries keep their **source-relative** paths regardless of the scope, so ignore rules match
/// exactly what they match in an unscoped scan. The walk descends only into the scope's subtree
/// and the ancestor chain leading to it, so its cost is proportional to the scope rather than to
/// the tree.
///
/// Directories on the ancestor chain are recorded even though they are not in scope: a scoped
/// upload still needs its parent directories created. They are excluded from status listings by
/// [`Scope::matches`], which admits only the scope itself and its descendants.
///
/// A scoped walk sees only scanned entries, so its portable-case-collision check covers the scope
/// alone. A collision outside the scope goes undetected — but is also untouched, so a scoped run
/// cannot create one. Callers must say so in their output rather than let a clean scoped result
/// read as a whole-tree clean bill of health.
pub fn scan_scoped(
    source: &Path,
    rules: &IgnoreRules,
    scope: &Scope,
    include_excluded: bool,
    budget: usize,
    cancellation: &CancellationToken,
) -> Result<ScopedScan> {
    cancellation.check()?;
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

    let context = ScanContext {
        rules,
        scope,
        include_excluded,
        budget,
        cancellation,
    };
    let mut output = ScanOutput::default();
    scan_dir(&root, "", &context, &mut output)?;
    if let Some((first, second)) = portable_case_collision(output.entries.keys()) {
        return Err(Error::UnsupportedLocalEntry {
            path: root.join(&second),
            reason: format!(
                "{first:?} and {second:?} differ only by case and cannot both sync to Windows Drive clients"
            ),
        });
    }
    Ok(ScopedScan {
        inventory: LocalInventory {
            root,
            entries: output.entries,
        },
        excluded: output.excluded,
        complete: output.complete,
    })
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

/// Hash every file, computing only MD5 for the entries a comparison will decide.
///
/// `comparison` names the files whose remote counterpart agrees on size and modification time.
/// Their remote digest comes from File Station's server-side MD5, which carries no CRC32 or
/// SHA-256, so computing those locally produces values that nothing can be compared against --
/// and SHA-256 is roughly 70% of the time this crate spends hashing. Everything else is hashed at
/// full strength, because it is already known to be an upload or a server-copy source, and those
/// consume a strong digest.
///
/// A comparison-set file whose digests disagree becomes an upload and therefore *does* need full
/// strength. That promotion is deliberately not attempted here -- which files differ is not known
/// until the plan is built -- and is completed by [`promote_to_full_fingerprint`]. Leaving it
/// undone is not a silent downgrade but a loud one: `full_match` returns `None` without strong
/// digests on both sides, and the upload verification in `api` treats that as a mismatch.
pub fn populate_content_md5_selective(
    inventory: &mut LocalInventory,
    comparison: &BTreeSet<String>,
    cancellation: &CancellationToken,
) -> Result<()> {
    for (relative, entry) in inventory.entries.iter_mut() {
        cancellation.check()?;
        if entry.kind != EntryKind::File {
            continue;
        }
        entry.content_md5 = Some(if comparison.contains(relative) {
            hash_file_md5_snapshot(entry, cancellation)?
        } else {
            hash_file_snapshot(entry, cancellation)?
        });
    }
    Ok(())
}

/// Recompute an entry's fingerprint at full strength.
///
/// Used to promote a file that was hashed for comparison only and then turned out to differ, so
/// that the upload path has the strong digest it verifies against. Re-reads the file, and the
/// snapshot checks inside the hash apply as they always do: a source that changed between the
/// comparison and the promotion is reported rather than uploaded on stale evidence.
pub fn promote_to_full_fingerprint(
    entry: &mut LocalEntry,
    cancellation: &CancellationToken,
) -> Result<()> {
    if entry
        .content_md5
        .is_some_and(|fingerprint| fingerprint.has_full_proof())
    {
        return Ok(());
    }
    entry.content_md5 = Some(hash_file_snapshot(entry, cancellation)?);
    Ok(())
}

pub fn hash_file_snapshot(
    entry: &LocalEntry,
    cancellation: &CancellationToken,
) -> Result<ContentMd5> {
    hash_file_with(
        entry,
        cancellation,
        ContentHasher::new(),
        |hasher, bytes| hasher.update(bytes),
    )
}

/// Compute only the MD5 component of `entry`'s fingerprint, with the same snapshot checks.
pub fn hash_file_md5_snapshot(
    entry: &LocalEntry,
    cancellation: &CancellationToken,
) -> Result<ContentMd5> {
    hash_file_with(
        entry,
        cancellation,
        Md5ContentHasher::new(),
        |hasher, bytes| hasher.update(bytes),
    )
}

/// Read `entry` once under its snapshot checks, feeding every byte to `hasher`.
///
/// The checks bracketing the read are the load-bearing part and are shared rather than duplicated
/// per digest strength: metadata before opening, metadata of the open handle, then both again
/// after the last byte. A file that changed underneath the read is reported as
/// [`Error::SourceChanged`] instead of yielding a digest of a mixture of two versions.
fn hash_file_with<H, F>(
    entry: &LocalEntry,
    cancellation: &CancellationToken,
    mut hasher: H,
    mut update: F,
) -> Result<ContentMd5>
where
    H: FingerprintHasher,
    F: FnMut(&mut H, &[u8]),
{
    verify_entry_snapshot(entry)?;
    let mut file = fs::File::open(&entry.full_path).map_err(|source| Error::FileIo {
        path: entry.full_path.clone(),
        source,
    })?;
    verify_open_snapshot(entry, &file)?;

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
        update(&mut hasher, &buffer[..count]);
    }
    verify_open_snapshot(entry, &file)?;
    verify_entry_snapshot(entry)?;
    Ok(hasher.finish())
}

/// The two fingerprint strengths, so the snapshot-checked read can be written once.
trait FingerprintHasher {
    fn finish(self) -> ContentMd5;
}

impl FingerprintHasher for ContentHasher {
    fn finish(self) -> ContentMd5 {
        self.finalize()
    }
}

impl FingerprintHasher for Md5ContentHasher {
    fn finish(self) -> ContentMd5 {
        self.finalize()
    }
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
    context: &ScanContext<'_>,
    output: &mut ScanOutput,
) -> Result<()> {
    let cancellation = context.cancellation;
    let rules = context.rules;
    cancellation.check()?;
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
        cancellation.check()?;
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
            if context.include_excluded && context.scope.matches(&relative) {
                output.excluded.insert(relative);
            }
            continue;
        }

        // Outside the scope entirely: neither the entry itself nor anything beneath it can be in
        // scope, so it is never opened. This is what keeps a scoped scan proportional to the
        // scope rather than to the tree.
        let in_scope = context.scope.matches(&relative);
        let on_scope_path = context.scope.is_ancestor_of_scope(&relative);
        if !in_scope && !on_scope_path {
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
            // Recorded as a single entry and not descended into, so asking for excluded paths
            // cannot turn a bounded scan into an unbounded one.
            if context.include_excluded && in_scope {
                output.excluded.insert(relative);
            }
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
            // An ancestor of the scope is recorded so a scoped plan can still create the parent
            // directories its uploads need. `Scope::matches` keeps it out of status listings.
            if !output.insert(
                relative.clone(),
                LocalEntry {
                    relative: relative.clone(),
                    full_path: full_path.clone(),
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_ms: 0,
                    content_md5: None,
                },
                context.budget,
            ) {
                return Ok(());
            }
            scan_dir(&full_path, &relative, context, output)?;
            if !output.complete {
                return Ok(());
            }
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
            // A file is only ever recorded when it is itself in scope. A file cannot be an
            // ancestor of anything, so a file sitting where the scope expects a directory simply
            // means the scope names nothing.
            if in_scope
                && !output.insert(
                    relative.clone(),
                    LocalEntry {
                        relative,
                        full_path,
                        kind: EntryKind::File,
                        size: metadata.len(),
                        mtime_ms,
                        content_md5: None,
                    },
                    context.budget,
                )
            {
                return Ok(());
            }
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
    if path.parent().is_some() {
        return Ok(());
    }
    let Some(share) = unc_share_root_name(path) else {
        return Err(Error::UnsupportedLocalEntry {
            path: path.to_owned(),
            reason: "the canonical source root cannot be a filesystem root".to_owned(),
        });
    };
    if is_administrative_share(&share) {
        return Err(Error::UnsupportedLocalEntry {
            path: path.to_owned(),
            reason: format!(
                "`{share}` is a Windows administrative share, which names a whole disk or a \
                 system endpoint rather than a folder of files"
            ),
        });
    }
    Ok(())
}

// An SMB share root has no parent, exactly like `/` or `C:\`, but a share is the network
// analogue of a folder rather than of a disk: `\\nas\media` is a perfectly ordinary thing to
// ask to sync, and refusing it while accepting `\\nas\media\photos` is arbitrary. A mapped
// drive letter is a disk-like alias, so `\\?\Z:\` stays rejected along with the real roots.
// A prefix that names a server but no share (`\\?\UNC\nas`) is not a directory anyone can
// sync either, so an empty share yields no name and is rejected too.
fn unc_share_root_name(path: &Path) -> Option<String> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    let share = match prefix.kind() {
        Prefix::UNC(_, share) | Prefix::VerbatimUNC(_, share) => share,
        _ => return None,
    };
    (!share.is_empty()).then(|| share.to_string_lossy().into_owned())
}

// Accepting share roots makes the share name load-bearing, and Windows publishes a family of
// shares that are disk- rather than folder-shaped: one per drive letter (`C$`), `ADMIN$` for
// the system directory, and `IPC$`, which is a named-pipe endpoint with no files behind it at
// all. Those are exactly what the filesystem-root guard exists to refuse, so they are named
// explicitly. Every other `$` share is allowed through: the suffix only marks a share as
// hidden from browsing, and hiding an ordinary data share (`backup$`) is a normal habit, so
// rejecting the whole suffix would reintroduce the false rejections that accepting share roots
// was meant to fix. Share names are case-insensitive on Windows, so `c$` is caught as well.
fn is_administrative_share(share: &str) -> bool {
    let Some(name) = share.strip_suffix('$') else {
        return false;
    };
    let drive_share =
        name.len() == 1 && name.starts_with(|letter: char| letter.is_ascii_alphabetic());
    drive_share || name.eq_ignore_ascii_case("admin") || name.eq_ignore_ascii_case("ipc")
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

    /// Walk one directory unscoped and unbudgeted, as the recursion does.
    fn walk_dir(
        directory: &Path,
        relative_parent: &str,
        rules: &IgnoreRules,
        cancellation: &CancellationToken,
        output: &mut ScanOutput,
    ) -> Result<()> {
        let scope = Scope::root();
        let context = ScanContext {
            rules,
            scope: &scope,
            include_excluded: false,
            budget: usize::MAX,
            cancellation,
        };
        scan_dir(directory, relative_parent, &context, output)
    }

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
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        let names: Vec<_> = inventory.entries.keys().cloned().collect();
        assert_eq!(names, ["a.txt", "keep", "keep/b.txt"]);
        assert_eq!(inventory.files(), 2);

        fs::remove_dir_all(root).unwrap();
    }

    /// A large tree is one of the longest phases a run has, and it used to be uninterruptible.
    /// The guard has to sit on the recursive walker itself, not only on `scan`'s entry check,
    /// or a Ctrl-C during the scan would be held until the whole tree had been read.
    #[test]
    fn scan_recursion_stops_at_the_next_directory_once_cancellation_arrives() {
        let root = temp_dir("scan-cancellation");
        fs::create_dir_all(root.join("alpha/nested")).unwrap();
        fs::create_dir(root.join("beta")).unwrap();
        fs::write(root.join("alpha/nested/a.txt"), b"a").unwrap();
        fs::write(root.join("beta/b.txt"), b"b").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();

        // A live token walks every level, so the tree itself is not what stops the scan below.
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        assert!(inventory.entries.contains_key("alpha/nested/a.txt"));
        assert!(inventory.entries.contains_key("beta/b.txt"));

        // Walk one subtree with a live token, then cancel and descend into the next exactly as
        // the recursion does. The walker refuses the directory and adds nothing to the results.
        let cancellation = CancellationToken::default();
        let mut output = ScanOutput::default();
        walk_dir(
            &root.join("alpha"),
            "alpha",
            &rules,
            &cancellation,
            &mut output,
        )
        .unwrap();
        let walked: Vec<_> = output.entries.keys().cloned().collect();
        assert_eq!(walked, ["alpha/nested", "alpha/nested/a.txt"]);

        cancellation.cancel();
        assert!(matches!(
            walk_dir(
                &root.join("beta"),
                "beta",
                &rules,
                &cancellation,
                &mut output
            ),
            Err(Error::Cancelled)
        ));
        assert_eq!(output.entries.keys().cloned().collect::<Vec<_>>(), walked);

        // The public entry point surfaces cancellation and never a partial inventory.
        assert!(matches!(
            scan(&root, &rules, &cancellation),
            Err(Error::Cancelled)
        ));

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
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
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
            scan(&root, &rules, &CancellationToken::default()),
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
            scan(&filesystem_root, &rules, &CancellationToken::default()),
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

    #[cfg(windows)]
    #[track_caller]
    fn assert_accepted_as_share_root(root: &Path) {
        assert!(
            reject_filesystem_root(root).is_ok(),
            "{} should be accepted as a share root",
            root.display()
        );
    }

    #[cfg(windows)]
    #[track_caller]
    fn assert_rejected_as_administrative_share(root: &Path, share: &str) {
        assert!(
            matches!(
                reject_filesystem_root(root),
                Err(Error::UnsupportedLocalEntry { path, reason })
                    if path == root
                        && reason.contains("administrative share")
                        && reason.contains(share)
            ),
            "{} should be rejected as an administrative share",
            root.display()
        );
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
            r"\\?\UNC\server\media",
            r"\\server\share",
        ] {
            assert_accepted_as_share_root(Path::new(accepted));
        }

        // A drive letter names a disk, not a folder, whether or not it is a mapped network
        // drive, and a UNC prefix naming no share is not a directory at all.
        for rejected in [r"\\?\C:\", r"C:\", r"\\?\Z:\", r"Z:\", r"\\?\UNC\server"] {
            assert_rejected_as_filesystem_root(rejected);
        }
    }

    // The administrative shares are the disk- and endpoint-shaped ones, so they stay rejected
    // even though they are spelled like any other share. Other hidden shares are ordinary data
    // shares wearing a `$`, and they keep the acceptance that share roots were given.
    #[cfg(windows)]
    #[test]
    fn rejects_administrative_share_roots_but_accepts_other_hidden_shares() {
        for (root, share) in [
            (r"\\?\UNC\localhost\C$", "C$"),
            (r"\\localhost\c$", "c$"),
            (r"\\?\UNC\server\D$", "D$"),
            (r"\\server\ADMIN$", "ADMIN$"),
            (r"\\?\UNC\server\admin$", "admin$"),
            (r"\\server\IPC$", "IPC$"),
            (r"\\?\UNC\server\ipc$", "ipc$"),
        ] {
            assert_rejected_as_administrative_share(Path::new(root), share);
        }

        // `backup$` is merely hidden, `CD$` is not a drive letter, and `C` is not hidden at
        // all: none of them name a disk, so none of them are the guard's business.
        for accepted in [
            r"\\?\UNC\server\backup$",
            r"\\server\backup$",
            r"\\?\UNC\server\CD$",
            r"\\?\UNC\server\C",
        ] {
            assert_accepted_as_share_root(Path::new(accepted));
        }
    }

    // A share root reaches the guard only after canonicalization, so prove the verdicts are
    // keyed to the spelling `fs::canonicalize` really produces rather than to a hand-written
    // approximation. `C$` is the one share an ordinary Windows host is sure to publish, so
    // borrow its canonical form and swap in an ordinary share name for the accepted case.
    #[cfg(windows)]
    #[test]
    fn judges_a_canonicalized_share_root_by_its_share_name() {
        let Ok(admin_share) = fs::canonicalize(r"\\localhost\C$") else {
            eprintln!(
                "skipping judges_a_canonicalized_share_root_by_its_share_name: no admin share"
            );
            return;
        };
        assert_eq!(admin_share.parent(), None);
        assert_rejected_as_administrative_share(&admin_share, "C$");

        let canonical = admin_share.to_string_lossy();
        let server = canonical
            .strip_suffix("C$")
            .expect("a canonical share root ends with its share name");
        assert_accepted_as_share_root(Path::new(&format!("{server}media")));
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
            scan(&link, &rules, &CancellationToken::default()),
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
            scan(&root, &rules, &CancellationToken::default()),
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
            scan(&root, &rules, &CancellationToken::default()),
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
        let mut inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();

        populate_content_md5(&mut inventory, &CancellationToken::default()).unwrap();
        let fingerprint = inventory.entries["payload.bin"].content_md5.unwrap();
        assert_eq!(fingerprint.to_string(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(fingerprint.crc32_hex().as_deref(), Some("352441c2"));
        assert_eq!(
            fingerprint.sha256_hex().as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
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
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
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
        let mut inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
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
            scan(&source_file, &rules, &CancellationToken::default()),
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
            scan(&missing, &rules, &CancellationToken::default()),
            Err(Error::FileIo { path, .. }) if path == missing
        ));

        let payload = root.join("payload.bin");
        fs::write(&payload, b"payload").unwrap();
        let entry = scan(&root, &rules, &CancellationToken::default())
            .unwrap()
            .entries["payload.bin"]
            .clone();
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
            scan(&root, &rules, &CancellationToken::default()),
            Err(Error::UnsupportedLocalEntry { reason, .. }) if reason.contains("UTF-8")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_dir_reports_the_missing_directory_it_could_not_read() {
        let root = temp_dir("scan-dir-missing");
        let missing = root.join("does-not-exist");
        let mut output = ScanOutput::default();
        let rules = IgnoreRules::build(&root, &[]).unwrap();

        assert!(matches!(
            walk_dir(
                &missing,
                "does-not-exist",
                &rules,
                &CancellationToken::default(),
                &mut output,
            ),
            Err(Error::FileIo { path, .. }) if path == missing
        ));
        assert!(output.entries.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hashing_rejects_a_file_replaced_by_a_link_since_the_scan() {
        let root = temp_dir("replaced-by-link");
        let target_dir = temp_dir("replaced-by-link-target");
        let replaced = root.join("replaced.bin");
        fs::write(&replaced, b"payload").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let entry = scan(&root, &rules, &CancellationToken::default())
            .unwrap()
            .entries["replaced.bin"]
            .clone();

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
            scan(&root, &rules, &CancellationToken::default()),
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
            scan(&root, &rules, &CancellationToken::default()),
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
        let entry = scan(&root, &rules, &CancellationToken::default())
            .unwrap()
            .entries["locked.bin"]
            .clone();

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
        let entry = scan(&root, &rules, &CancellationToken::default())
            .unwrap()
            .entries["secret.bin"]
            .clone();

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
            scan(&root, &rules, &CancellationToken::default()),
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

#[cfg(test)]
mod scoped_scan_tests {
    use std::fs;

    use super::*;

    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sdsync-scoped-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        for directory in ["docs/q3", "docs/q4", "other/deep", "cache"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        fs::write(root.join("top.txt"), "t").unwrap();
        fs::write(root.join("docs/a.txt"), "a").unwrap();
        fs::write(root.join("docs/q3/summary.pdf"), "s").unwrap();
        fs::write(root.join("docs/q4/draft.pdf"), "d").unwrap();
        fs::write(root.join("other/deep/far.txt"), "f").unwrap();
        fs::write(root.join("cache/big.tmp"), "c").unwrap();
        root
    }

    fn walk(root: &Path, scope: &str, patterns: &[&str]) -> ScopedScan {
        let rules = IgnoreRules::build(
            root,
            &patterns
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let scope = Scope::parse(scope).unwrap();
        scan_scoped(
            root,
            &rules,
            &scope,
            false,
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap()
    }

    fn keys(scan: &ScopedScan) -> Vec<String> {
        scan.inventory.entries.keys().cloned().collect()
    }

    #[test]
    fn an_unscoped_scan_is_unchanged_by_the_scoping_machinery() {
        let root = tree("unscoped");
        let scan = walk(&root, "", &[]);
        assert_eq!(
            keys(&scan),
            [
                "cache",
                "cache/big.tmp",
                "docs",
                "docs/a.txt",
                "docs/q3",
                "docs/q3/summary.pdf",
                "docs/q4",
                "docs/q4/draft.pdf",
                "other",
                "other/deep",
                "other/deep/far.txt",
                "top.txt",
            ]
        );
        assert!(scan.complete);
        assert!(scan.excluded.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_directory_scope_walks_only_its_subtree() {
        let root = tree("subtree");
        let scan = walk(&root, "docs/q3", &[]);
        // The ancestor chain is recorded so a scoped plan can still create parents; nothing
        // outside the scope or its ancestors is opened at all.
        assert_eq!(keys(&scan), ["docs", "docs/q3", "docs/q3/summary.pdf"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_scope_records_the_file_and_only_its_ancestor_directories() {
        let root = tree("file-scope");
        let scan = walk(&root, "docs/q3/summary.pdf", &[]);
        assert_eq!(keys(&scan), ["docs", "docs/q3", "docs/q3/summary.pdf"]);
        // The sibling in the same directory is never recorded.
        assert!(!scan.inventory.entries.contains_key("docs/a.txt"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_scope_that_does_not_exist_yields_only_the_ancestors_that_do() {
        let root = tree("absent");
        let scan = walk(&root, "docs/q9/missing.txt", &[]);
        assert_eq!(keys(&scan), ["docs"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scoped_relatives_stay_source_relative_so_ignore_rules_still_match() {
        let root = tree("ignore");
        // The pattern is written against the source-relative path, as it always is.
        let scan = walk(&root, "docs", &["docs/q4/**"]);
        assert_eq!(
            keys(&scan),
            [
                "docs",
                "docs/a.txt",
                "docs/q3",
                "docs/q3/summary.pdf",
                "docs/q4"
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn excluded_paths_are_reported_only_when_requested() {
        let root = tree("excluded");
        let rules = IgnoreRules::build(&root, &["cache/**".to_owned()]).unwrap();
        let scope = Scope::root();

        let quiet = scan_scoped(
            &root,
            &rules,
            &scope,
            false,
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(quiet.excluded.is_empty());

        let loud = scan_scoped(
            &root,
            &rules,
            &scope,
            true,
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(loud.excluded.contains("cache/big.tmp"));
        // The excluded entry is recorded but never becomes part of the payload inventory.
        assert!(!loud.inventory.entries.contains_key("cache/big.tmp"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_excluded_directory_is_recorded_once_and_not_descended_into() {
        let root = tree("excluded-dir");
        let rules = IgnoreRules::build(&root, &["other/".to_owned()]).unwrap();
        let scope = Scope::root();
        let scan = scan_scoped(
            &root,
            &rules,
            &scope,
            true,
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(scan.excluded.contains("other"));
        // Recording the pruned directory must not turn a bounded scan into a walk of its contents.
        assert!(!scan.excluded.iter().any(|path| path.starts_with("other/")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_budget_stops_the_walk_and_says_the_result_is_partial() {
        let root = tree("budget");
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let scope = Scope::root();
        let scan = scan_scoped(
            &root,
            &rules,
            &scope,
            false,
            3,
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(
            !scan.complete,
            "an exhausted budget must report a partial scan"
        );
        assert!(scan.inventory.entries.len() <= 3);

        // The same tree within budget completes.
        let full = scan_scoped(
            &root,
            &rules,
            &scope,
            false,
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(full.complete);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_scoped_scan_is_cancellable() {
        let root = tree("cancel");
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let scope = Scope::parse("docs").unwrap();
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        assert!(matches!(
            scan_scoped(&root, &rules, &scope, false, usize::MAX, &cancellation),
            Err(Error::Cancelled)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_public_scan_entry_point_still_returns_the_whole_tree() {
        let root = tree("public");
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        assert_eq!(inventory.entries.len(), 12);
        assert_eq!(inventory.files(), 6);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod fingerprint_strength_tests {
    use std::fs;

    use super::*;

    /// Build a two-file tree and scan it, so the entries carry real metadata snapshots.
    fn scanned(name: &str) -> (PathBuf, LocalInventory) {
        let root = std::env::temp_dir().join(format!(
            "sdsync-strength-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("compared.bin"), b"compared").unwrap();
        fs::write(root.join("uploaded.bin"), b"uploaded").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        (root, inventory)
    }

    /// Comparison-set files are hashed MD5-only; everything else keeps full strength.
    ///
    /// The narrowing that made content mode affordable locally as well as remotely. A file whose
    /// remote counterpart agrees on size and modification time has its digest compared against
    /// File Station's server-side MD5, which carries no CRC32 or SHA-256 -- so computing those
    /// locally produces values nothing can be compared against, and SHA-256 alone is roughly 70%
    /// of the time this crate spends hashing. A file with no such counterpart is already known to
    /// be an upload, and uploads consume a strong digest, so it is hashed in full from the start.
    #[test]
    fn only_comparison_set_files_are_hashed_md5_only() {
        let (root, mut inventory) = scanned("selective");
        let comparison = BTreeSet::from(["compared.bin".to_owned()]);
        populate_content_md5_selective(&mut inventory, &comparison, &CancellationToken::default())
            .unwrap();

        let compared = inventory.entries["compared.bin"].content_md5.unwrap();
        let uploaded = inventory.entries["uploaded.bin"].content_md5.unwrap();
        assert!(
            !compared.has_full_proof(),
            "a comparison-set file must not pay for digests nothing will compare"
        );
        assert!(
            uploaded.has_full_proof(),
            "a file outside the comparison set is an upload and needs strong proof"
        );
        // Narrowing changes which digests are computed, never the MD5 that is.
        assert_eq!(
            compared.as_bytes(),
            ContentMd5::from_content(b"compared").as_bytes()
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Promotion raises a comparison-set digest to full strength, and leaves a complete one alone.
    ///
    /// The counterpart to the narrowing above. A comparison-set file that turns out to differ
    /// becomes an upload, and the upload path verifies with `full_match`, which cannot answer
    /// unless both sides carry SHA-256. Promotion closes that gap. Skipping it does not produce an
    /// unverified upload but a failed one, which is the right direction to fail in -- and is
    /// covered end to end by `content_mode_still_detects_a_change_hidden_behind_equal_size_and_mtime`.
    #[test]
    fn promotion_raises_an_md5_only_digest_and_leaves_a_complete_one_untouched() {
        let (root, mut inventory) = scanned("promotion");
        let cancellation = CancellationToken::default();
        populate_content_md5_selective(
            &mut inventory,
            &BTreeSet::from(["compared.bin".to_owned()]),
            &cancellation,
        )
        .unwrap();

        let entry = inventory.entries.get_mut("compared.bin").unwrap();
        assert!(!entry.content_md5.unwrap().has_full_proof());
        promote_to_full_fingerprint(entry, &cancellation).unwrap();
        let promoted = entry.content_md5.unwrap();
        assert!(
            promoted.has_full_proof(),
            "a promoted digest must carry the components an upload verifies against"
        );
        assert_eq!(promoted, ContentMd5::from_content(b"compared"));

        // Idempotent: promoting a digest that is already complete must not change it.
        promote_to_full_fingerprint(entry, &cancellation).unwrap();
        assert_eq!(entry.content_md5.unwrap(), promoted);

        // And a file hashed at full strength from the start is already promoted.
        let strong = inventory.entries.get_mut("uploaded.bin").unwrap();
        let before = strong.content_md5.unwrap();
        promote_to_full_fingerprint(strong, &cancellation).unwrap();
        assert_eq!(strong.content_md5.unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }
}
