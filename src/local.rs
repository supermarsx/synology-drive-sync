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

/// Filesystem identity of a scanned entry, where the platform exposes one.
///
/// Captured during the walk because the walk already holds the [`fs::Metadata`] it comes from.
/// Obtaining it later would mean a second stat of every entry, which is a cost comparable to the
/// walk itself — and on the armv7 target that cost is the thing the status cache exists to avoid.
///
/// [`Self::is_known`] is false where nothing is exposed, and two unknown identities comparing equal
/// establishes nothing. Consumers guard against that by never comparing identities across
/// platforms; the status cache records the platform it was written on and refuses a file from
/// another.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

impl FileIdentity {
    pub fn is_known(self) -> bool {
        self != Self::default()
    }
}

/// Read the filesystem identity out of metadata the caller already holds.
///
/// Unix only. Windows exposes a volume serial and file index, but only when the metadata came from
/// an open handle rather than a path stat, so a scan built on `symlink_metadata` would silently get
/// nothing for most entries — an identity that is sometimes present is worse than one that is
/// consistently absent, because only the second is safe to reason about.
#[cfg(unix)]
pub fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
pub fn file_identity(_metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity::default()
}

#[derive(Clone, Debug)]
pub struct LocalEntry {
    pub relative: String,
    pub full_path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: i64,
    /// Device and inode, where the platform exposes them. Default elsewhere, and default for
    /// directories, which are never content-compared.
    pub identity: FileIdentity,
    pub content_md5: Option<ContentMd5>,
}

#[derive(Debug)]
pub struct LocalInventory {
    pub root: PathBuf,
    pub entries: BTreeMap<String, LocalEntry>,
    /// Paths the walk saw and declined to carry, and why.
    ///
    /// Not an exclusion: nobody asked for these to be left out, so they are reported. And not an
    /// absence either. A skipped path exists locally, so the planner treats it and everything
    /// beneath it as protected and never schedules a mirror deletion there.
    pub skipped: Vec<SkippedEntry>,
}

/// One path a run did not carry although nothing excluded it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedEntry {
    /// Source-relative path, in the local spelling where one exists and the remote one otherwise.
    pub relative: String,
    pub reason: SkipReason,
}

/// Why a path was skipped instead of failing the whole run.
///
/// Each of these used to be a whole-run refusal, so one `desktop.ini` or one case-only rename
/// anywhere in a tree meant nothing synced at all, on every run, until someone found and fixed it
/// by hand. A skip carries only the affected path and leaves both sides of it untouched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkipReason {
    /// A Windows SYSTEM or TEMPORARY entry that the default exclusions did not cover.
    WindowsAttributes,
    /// A symbolic link, junction or volume mount point -- an entry that redirects to another path --
    /// or a reparse point with no data behind it at all (an app execution alias, an `AF_UNIX`
    /// socket).
    /// Reparse points that hold their own data (deduplicated files, hydrated or pinned OneDrive
    /// files) are not this; they are carried as ordinary files.
    LinkOrReparsePoint,
    /// A cloud or HSM placeholder whose content is not on this disk (`RECALL_ON_DATA_ACCESS`,
    /// `RECALL_ON_OPEN` or `OFFLINE`). Reading it would start a download, so it is not read.
    DehydratedPlaceholder,
    /// Two local names differ only by case or Unicode normalisation.
    LocalCaseCollision { other: String },
    /// The destination already holds a second spelling of this path.
    RemoteCaseCollision { other: String },
    /// A case-only rename. Carrying it means removing the old remote spelling, which only a
    /// mirror run (`--delete`) may do.
    CaseRenameNeedsDelete { remote: String },
    /// Withheld after the walk because it could not be read consistently, with the error that said
    /// so. See [`LocalInventory::withhold`].
    Deferred { detail: String },
}

impl SkipReason {
    /// Stable machine tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WindowsAttributes => "windows-attributes",
            Self::LinkOrReparsePoint => "link-or-reparse-point",
            Self::DehydratedPlaceholder => "dehydrated-placeholder",
            Self::LocalCaseCollision { .. } => "local-case-collision",
            Self::RemoteCaseCollision { .. } => "remote-case-collision",
            Self::CaseRenameNeedsDelete { .. } => "case-rename-needs-delete",
            Self::Deferred { .. } => "deferred",
        }
    }

    /// One human sentence, naming the other spelling where there is one.
    pub fn detail(&self) -> String {
        match self {
            Self::WindowsAttributes => {
                "Windows SYSTEM and TEMPORARY entries are not synchronized by Synology Drive"
                    .to_owned()
            }
            Self::LinkOrReparsePoint => {
                "symbolic links, junctions, mount points and data-less reparse points (app \
                 execution aliases, sockets) are not followed"
                    .to_owned()
            }
            Self::DehydratedPlaceholder => {
                "an online-only placeholder whose content is not on this disk; reading it would                  download it, so it is skipped until it is made available offline"
                    .to_owned()
            }
            Self::LocalCaseCollision { other } => format!(
                "the source also holds {other:?}; names that differ only by case or Unicode \
                 normalisation cannot coexist on Synology Drive clients, so neither is synchronized"
            ),
            Self::RemoteCaseCollision { other } => format!(
                "the destination also holds {other:?}; names that differ only by case or Unicode \
                 normalisation cannot coexist on Synology Drive clients, so neither is changed"
            ),
            Self::CaseRenameNeedsDelete { remote } => format!(
                "a case-only rename of {remote:?}; run with --delete to replace the old spelling, \
                 or rename it in File Station"
            ),
            Self::Deferred { detail } => format!("left for the next run: {detail}"),
        }
    }
}

/// OS metadata files no client means to synchronize, excluded unless a rule re-includes them.
///
/// Synology Drive's own client skips these. Without the defaults, Windows' `desktop.ini` --
/// Hidden+System in every Documents, Pictures, Music and Desktop folder -- failed every scan of a
/// standard user folder, and macOS's `Icon\r` (a control character in a name) did the same for any
/// folder with a custom icon. Written as gitignore patterns so a later `!desktop.ini` in
/// `.sdsyncignore` or `--exclude` still wins, and spelled case-insensitively where Windows itself
/// varies the case. `Icon[\r]` is a character class because a trailing `\r` would be trimmed as
/// whitespace; the class keeps it and still never matches a plain `Icon`.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "[Dd][Ee][Ss][Kk][Tt][Oo][Pp].[Ii][Nn][Ii]",
    "[Tt][Hh][Uu][Mm][Bb][Ss].[Dd][Bb]",
    "[Ee][Hh][Tt][Hh][Uu][Mm][Bb][Ss].[Dd][Bb]",
    ".DS_Store",
    "Icon[\r]",
    "~$*",
];

impl LocalInventory {
    /// Take a path out of this run without letting it read as absent.
    ///
    /// For a file the run cannot carry after all -- it changed while being hashed, or could not be
    /// read -- discovered after the walk. Removing the entry alone would be wrong in the dangerous
    /// direction: the planner would see a remote file with no local counterpart and, under
    /// `--delete`, delete it. Recorded as skipped instead, the planner shields the path and every
    /// remote spelling of it (case or Unicode normalisation variants included), keeps its remote
    /// ancestors, and reports it. The remote inventory must be left as it is: shielding needs the
    /// remote entry present to protect its parents.
    pub fn withhold(&mut self, relative: &str, reason: SkipReason) {
        let prefix = format!("{relative}/");
        let beneath: Vec<String> = self
            .entries
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .take_while(|(candidate, _)| candidate.starts_with(&prefix))
            .map(|(candidate, _)| candidate.clone())
            .collect();
        for candidate in beneath {
            self.entries.remove(&candidate);
        }
        self.entries.remove(relative);
        self.skipped.push(SkippedEntry {
            relative: relative.to_owned(),
            reason,
        });
        self.skipped
            .sort_by(|left, right| left.relative.cmp(&right.relative));
    }

    pub fn files(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.kind == EntryKind::File)
            .count()
    }
}

/// The exclusion rules one run was built with.
///
/// Cloneable, and cheaply: a [`SyncPlan`](crate::plan::SyncPlan) carries the rules it was planned
/// under so the executor's own inventory can prune exactly what the planner's did. Without that the
/// two walks disagree about which entries exist.
#[derive(Clone)]
pub struct IgnoreRules {
    matcher: Gitignore,
}

impl std::fmt::Debug for IgnoreRules {
    /// The compiled patterns are not rendered: `Gitignore` is not `Debug`, and a plan's `Debug`
    /// output is read to see what a run would do, which the rules describe only by exclusion.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IgnoreRules")
            .field("patterns", &self.matcher.num_ignores())
            .finish()
    }
}

impl IgnoreRules {
    pub fn build(source: &Path, extra_patterns: &[String]) -> Result<Self> {
        let source_metadata =
            fs::symlink_metadata(source).map_err(|source_error| Error::FileIo {
                path: source.to_owned(),
                source: source_error,
            })?;
        if is_link_or_reparse(source, &source_metadata) {
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
        // First, so that every operator rule -- the ignore file's and the command line's -- is
        // later and can negate one of them.
        for pattern in DEFAULT_EXCLUDES {
            builder.add_line(None, pattern).map_err(|error| {
                Error::Message(format!("invalid built-in exclude {pattern:?}: {error}"))
            })?;
        }
        let default_file = source_root.join(DEFAULT_IGNORE_FILE);
        match fs::symlink_metadata(&default_file) {
            Ok(metadata) => {
                if is_link_or_reparse(&default_file, &metadata) || !metadata.is_file() {
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

    /// Rules that exclude nothing.
    ///
    /// For a caller with no source tree to read patterns from: a remote inventory taken by a
    /// diagnostic, or a fixture. `.sdsyncignore` stays excluded, because that is a property of the
    /// name and not of any pattern.
    pub fn none() -> Self {
        Self {
            matcher: Gitignore::empty(),
        }
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
    skipped: Vec<SkippedEntry>,
    complete: bool,
}

impl Default for ScanOutput {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            excluded: BTreeSet::new(),
            skipped: Vec::new(),
            complete: true,
        }
    }
}

impl ScanOutput {
    /// Record an entry, reporting whether the walk may continue.
    ///
    /// The budget is the number of entries kept, so the entry that proves the walk was truncated is
    /// the first one that does **not** fit — not the last one that did. Checking after the insert
    /// instead meant a tree of exactly the budget stopped with `complete = false`, and a reader is
    /// required to render an incomplete total as a floor: a 20,000-entry tree at the shipped
    /// 20,000-entry budget was published as "20,000+" when it was in fact all of it.
    fn insert(&mut self, relative: String, entry: LocalEntry, budget: usize) -> bool {
        if self.recorded() >= budget {
            self.complete = false;
            return false;
        }
        self.entries.insert(relative, entry);
        true
    }

    /// Record a path the walk declines to carry, on the same budget as an entry.
    ///
    /// Charged against the budget because a skip is held in memory exactly like an entry, and a
    /// tree that is nothing but cloud placeholders would otherwise be an unbounded list.
    fn skip(&mut self, relative: String, reason: SkipReason, budget: usize) -> bool {
        if self.recorded() >= budget {
            self.complete = false;
            return false;
        }
        self.skipped.push(SkippedEntry { relative, reason });
        true
    }

    fn recorded(&self) -> usize {
        self.entries.len().saturating_add(self.skipped.len())
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
    if is_link_or_reparse(source, &source_metadata) {
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
    skip_portable_case_collisions(&mut output);
    output
        .skipped
        .sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(ScopedScan {
        inventory: LocalInventory {
            root,
            entries: output.entries,
            skipped: output.skipped,
        },
        excluded: output.excluded,
        complete: output.complete,
    })
}

/// Read one source-relative path that the walk did not record, applying the walk's own rules.
///
/// For the one case where a path has to be inspected after the scan: a mirror deletion that was
/// refused because the local file reappeared, which the run then uploads so it converges instead of
/// deferring to the next run. Scanning the whole tree again to learn one file's size would cost the
/// tree; doing it by hand would drop the validation the walk performs, and a path that reappeared as
/// a symlink, a device node or a name DSM will not take must be refused here exactly as it would
/// have been refused there.
///
/// `Ok(None)` means there is nothing to record: the path is absent again, or a DSM-managed
/// component excludes it. Every other disagreement with the walk is an error.
///
/// Ignore rules are deliberately **not** applied here, and the caller must already have established
/// that the path is not excluded. The one caller does: a mirror deletion candidate has passed
/// `is_protected`, which applies the very same rules to the very same relative path with the same
/// `is_dir` flag. Taking a second `&IgnoreRules` would let the two answers drift apart, and there is
/// no other call site to serve.
pub fn entry_at(root: &Path, relative: &str) -> Result<Option<LocalEntry>> {
    if relative.is_empty() {
        return Ok(None);
    }
    if is_dsm_managed(relative) {
        return Ok(None);
    }
    let full_path = root.join(relative);
    let metadata = match fs::symlink_metadata(&full_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::FileIo {
                path: full_path,
                source,
            });
        }
    };
    validate_relative(relative)?;
    if let Some(reason) = drive_path_issue(relative) {
        return Err(Error::UnsupportedLocalEntry {
            path: full_path,
            reason,
        });
    }
    // The walk skips these rather than refusing them, so there is nothing here to record either.
    // The caller's deletion stays kept: the path exists, it is only not carried.
    if is_link_or_reparse(&full_path, &metadata) || has_unsupported_windows_attributes(&metadata) {
        return Ok(None);
    }
    if metadata.is_dir() {
        return Ok(Some(LocalEntry {
            relative: relative.to_owned(),
            full_path,
            kind: EntryKind::Directory,
            size: 0,
            mtime_ms: 0,
            identity: FileIdentity::default(),
            content_md5: None,
        }));
    }
    if !metadata.is_file() {
        return Err(Error::UnsupportedLocalEntry {
            path: full_path,
            reason: "only regular files and directories can be synchronized".to_owned(),
        });
    }
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
    Ok(Some(LocalEntry {
        relative: relative.to_owned(),
        size: metadata.len(),
        mtime_ms,
        identity: file_identity(&metadata),
        kind: EntryKind::File,
        full_path,
        content_md5: None,
    }))
}

pub fn populate_content_md5(
    inventory: &mut LocalInventory,
    cancellation: &CancellationToken,
) -> Result<()> {
    for entry in inventory.entries.values_mut() {
        cancellation.check()?;
        cancellation.tick();
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
/// An entry that already carries a digest is left alone, and that is the hook the status digest
/// cache hangs on: it fills `content_md5` for files it can prove are unchanged, and those files are
/// then not re-read here.
///
/// Signalling through the field rather than through an explicit set of paths is a memory decision,
/// not a stylistic one. A set naming the supplied paths would hold every path a third time —
/// roughly 5 MB at the scan budget — on top of the two inventories that `tests/scan_memory.rs`
/// pins at 27 MiB of a 32 MiB ceiling, and would breach it on the armv7 target the cache exists to
/// help. The field already carries the information; a second copy of it is what does not fit.
///
/// Skipping is only safe because of what the supplier proved before filling the field: that the
/// file's size, modification time and filesystem identity are unchanged since the digest was taken.
/// That is the same evidence [`verify_metadata_snapshot`] accepts as proof a file did not change
/// underneath a read — held across runs rather than across one read, which is the whole of the
/// difference, and the reason no mutation path may fill this field from stored evidence.
///
/// Every caller other than the cache passes a freshly scanned inventory, where every digest is
/// `None`, so for them this is exactly what it always was.
pub fn populate_content_md5_selective(
    inventory: &mut LocalInventory,
    comparison: &BTreeSet<String>,
    cancellation: &CancellationToken,
) -> Result<()> {
    for (relative, entry) in inventory.entries.iter_mut() {
        cancellation.check()?;
        // The one per-file boundary this pass has, and the phase a content-mode status walk
        // spends most of its time in. The read loop inside the hash checks cancellation too, so
        // counting there would report chunks rather than files.
        cancellation.tick();
        if entry.kind != EntryKind::File {
            continue;
        }
        if entry.content_md5.is_some() {
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

/// [`populate_content_md5_selective`], returning the files that could not be hashed instead of
/// failing on the first one.
///
/// A file that changed, vanished or would not open while being hashed is a fact about that file,
/// not about the run: one open mailbox or growing log used to stop every other file from syncing.
/// Each failure is returned with its error and the entry is left without a digest; the caller must
/// take it out of the run with [`LocalInventory::withhold`], never by deleting the entry, which
/// would make its remote copy look like a mirror-deletion candidate. Cancellation still ends the
/// whole pass and returns no partial result.
pub fn populate_content_md5_selective_deferring(
    inventory: &mut LocalInventory,
    comparison: &BTreeSet<String>,
    cancellation: &CancellationToken,
) -> Result<Vec<(String, Error)>> {
    let mut failed = Vec::new();
    for (relative, entry) in inventory.entries.iter_mut() {
        cancellation.check()?;
        cancellation.tick();
        if entry.kind != EntryKind::File || entry.content_md5.is_some() {
            continue;
        }
        let digest = if comparison.contains(relative) {
            hash_file_md5_snapshot(entry, cancellation)
        } else {
            hash_file_snapshot(entry, cancellation)
        };
        match digest {
            Ok(digest) => entry.content_md5 = Some(digest),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => failed.push((relative.clone(), error)),
        }
    }
    Ok(failed)
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
    if is_link_or_reparse(&entry.full_path, &metadata) {
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
        cancellation.tick();
        let name = match child.file_name().into_string() {
            Ok(name) => name,
            Err(raw) => {
                // A name that cannot be represented as UTF-8 cannot be mapped to a remote path, so
                // refusing it is right. Refusing it *after* the operator excluded it is not. The
                // rules are matched against the lossy spelling to decide exclusion only; a name
                // that survives that still fails the scan, because there is no spelling of it that
                // could be uploaded.
                let lossy = raw.to_string_lossy().into_owned();
                let relative = if relative_parent.is_empty() {
                    lossy
                } else {
                    format!("{relative_parent}/{lossy}")
                };
                let is_dir = child
                    .file_type()
                    .map_err(|source| Error::FileIo {
                        path: child.path(),
                        source,
                    })?
                    .is_dir();
                if is_dsm_managed(&relative) || rules.is_ignored(&relative, is_dir) {
                    if context.include_excluded && context.scope.matches(&relative) {
                        output.excluded.insert(relative);
                    }
                    continue;
                }
                return Err(Error::UnsupportedLocalEntry {
                    path: child.path(),
                    reason: "name is not valid UTF-8".to_owned(),
                });
            }
        };
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

        // Excluded before validated, and the order is the finding. These two checks refuse names
        // the tool will not carry -- anything beginning with `~`, a Windows device stem like
        // `aux.h`, a component over 255 characters -- by failing the *whole scan*, so one such name
        // anywhere in the tree meant nothing was uploaded. Running them before the rules made that
        // unfixable by the operator: Word opens `report.docx`, creates `~$report.docx` beside it,
        // and every scheduled run fails until the document is closed, with `--exclude '~$*'` having
        // no effect because the refusal happened first. Excluding an ancestor *directory* did work,
        // because a directory's own name passes validation and the walk never descends -- so the
        // remedy that worked was unrelated in shape to the remedy the error invited, and there was
        // no remedy at all when the offending file sat beside files that had to be synced.
        //
        // An entry the rules exclude is not going anywhere, so whether its name could be carried is
        // not a question this run has to answer.
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

        validate_relative(&relative)?;
        if let Some(reason) = drive_path_issue(&relative) {
            return Err(Error::UnsupportedLocalEntry {
                path: child.path(),
                reason,
            });
        }
        let metadata = fs::symlink_metadata(child.path()).map_err(|source| Error::FileIo {
            path: child.path(),
            source,
        })?;
        // Skipped, not refused, and never descended into. These used to fail the whole scan, and
        // ordinary trees are full of them: every Files-On-Demand OneDrive file is a reparse point,
        // and Windows marks assorted shell files SYSTEM. One of them meant nothing synced. The skip
        // is recorded, so it is reported and the planner protects the path from mirror deletion.
        let skip = if let Some(reason) = link_skip_reason(&child.path(), &metadata) {
            Some(reason)
        } else if has_unsupported_windows_attributes(&metadata) {
            Some(SkipReason::WindowsAttributes)
        } else {
            None
        };
        if let Some(reason) = skip {
            if !output.skip(relative, reason, context.budget) {
                return Ok(());
            }
            continue;
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
                    identity: FileIdentity::default(),
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
                        identity: file_identity(&metadata),
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

/// Take every pair of local names that differ only by case or Unicode normalisation out of the
/// inventory, with everything beneath them, and record each spelling as skipped.
///
/// A case-sensitive source can hold `Report.pdf` beside `report.pdf`, and a Linux one can hold an
/// NFC and an NFD spelling of `café`. No Synology Drive client can hold both, so neither may be
/// carried -- but that used to fail the whole scan, so one such pair anywhere stopped every file
/// from syncing. Both spellings are dropped here and nothing else is touched: the skip records make
/// the planner treat both, and every remote spelling of them, as protected.
fn skip_portable_case_collisions(output: &mut ScanOutput) {
    let mut by_fold: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for relative in output.entries.keys() {
        by_fold
            .entry(crate::path::portable_fold(relative))
            .or_default()
            .push(relative);
    }
    let mut colliding: Vec<(String, String)> = Vec::new();
    let mut dropped_folds: BTreeSet<String> = BTreeSet::new();
    for (fold, spellings) in &by_fold {
        if spellings.len() < 2 {
            continue;
        }
        // Beneath a pair already dropped: its subtree goes with it, and one report is enough.
        if fold_has_ancestor_in(fold, &dropped_folds) {
            continue;
        }
        dropped_folds.insert(fold.clone());
        for (index, spelling) in spellings.iter().enumerate() {
            let other = spellings[(index + 1) % spellings.len()];
            colliding.push(((*spelling).clone(), other.clone()));
        }
    }
    drop(by_fold);
    for (relative, other) in colliding {
        let prefix = format!("{relative}/");
        let beneath: Vec<String> = output
            .entries
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .take_while(|(candidate, _)| candidate.starts_with(&prefix))
            .map(|(candidate, _)| candidate.clone())
            .collect();
        for candidate in beneath {
            output.entries.remove(&candidate);
        }
        output.entries.remove(&relative);
        output.skipped.push(SkippedEntry {
            relative,
            reason: SkipReason::LocalCaseCollision { other },
        });
    }
}

/// True when a strict ancestor of the folded path `fold` is in `folds`.
pub(crate) fn fold_has_ancestor_in(fold: &str, folds: &BTreeSet<String>) -> bool {
    if folds.is_empty() {
        return false;
    }
    fold.match_indices('/')
        .any(|(index, _)| folds.contains(&fold[..index]))
}

/// Whether an entry holds its own data here, points somewhere else, or holds none locally yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinkClass {
    /// An ordinary file or directory -- including a Windows reparse point whose data is local:
    /// a Data Deduplication file, a hydrated or pinned OneDrive file, a WOF-compressed file.
    Ordinary,
    /// A symbolic link, junction or volume mount point. Never followed.
    Link,
    /// A cloud or HSM placeholder whose content is not on this disk. Reading it would start a
    /// download or a recall, so it is never read.
    DehydratedPlaceholder,
}

const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Classify a Windows entry from what `symlink_metadata` reports.
///
/// `name_surrogate` is the standard library's `FileType::is_symlink`, which on Windows is true
/// exactly for reparse points whose tag is a *name surrogate*: symbolic links, junctions and volume
/// mount points, the reparse kinds that redirect to another path. Every other reparse tag --
/// `IO_REPARSE_TAG_DEDUP`, the `IO_REPARSE_TAG_CLOUD_*` family, WOF -- stores this file's own data
/// behind a filter driver and reads like any file.
///
/// Skipping every reparse point used to drop all of those silently: a deduplicated volume or a
/// OneDrive folder reported a complete, successful run with those files never synced. Only the
/// recall attributes say whether reading would leave the disk, so they decide; the tag never could
/// on its own, because a cloud file carries the same tag hydrated or not.
fn classify_link(name_surrogate: bool, attributes: u32) -> LinkClass {
    if name_surrogate {
        return LinkClass::Link;
    }
    if attributes
        & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_OFFLINE)
        != 0
    {
        return LinkClass::DehydratedPlaceholder;
    }
    // Whether or not FILE_ATTRIBUTE_REPARSE_POINT is set: a reparse point that is neither a name
    // surrogate nor awaiting recall holds its data here.
    LinkClass::Ordinary
}

/// Windows' answer to opening a reparse point no filter driver serves: `ERROR_CANT_ACCESS_FILE`
/// (1920) and `ERROR_CANT_RESOLVE_FILENAME` (1921).
#[cfg(windows)]
const UNOPENABLE_REPARSE_ERRORS: [i32; 2] = [1920, 1921];

/// Whether an open that failed says the entry is a reparse point with no data behind it.
#[cfg(windows)]
fn is_unopenable_reparse_error(error: &std::io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| UNOPENABLE_REPARSE_ERRORS.contains(&code))
}

/// [`classify_link`], plus one probe the attributes cannot replace.
///
/// Some non-surrogate reparse tags carry no data at all: an app execution alias
/// (`IO_REPARSE_TAG_APPEXECLINK`, the `python.exe` in `WindowsApps`) or an `AF_UNIX` socket
/// (`IO_REPARSE_TAG_AF_UNIX`). They are not name surrogates and not awaiting recall, so they read as
/// ordinary -- and then every open fails with 1920, which turned each run that met one into a
/// partial one. The tag would say so directly, but this crate forbids the `unsafe` needed to read
/// it. Opening the file says it just as well, and costs one open per non-surrogate reparse *file*,
/// which outside a deduplicated or OneDrive tree is none at all. Directories are not probed: a
/// directory cannot be opened as a file, and its children are classified one by one anyway.
/// (`IO_REPARSE_TAG_LX_SYMLINK` is a name surrogate, so it is already a link without the probe.)
///
/// Any other open error is not decided here: the entry stays ordinary and the hash that reads it
/// reports the error against the path, as for any file.
#[cfg(windows)]
fn link_class(path: &Path, metadata: &fs::Metadata) -> LinkClass {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let class = classify_link(
        metadata.file_type().is_symlink(),
        metadata.file_attributes(),
    );
    if class == LinkClass::Ordinary
        && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && !metadata.is_dir()
        && fs::File::open(path)
            .err()
            .is_some_and(|error| is_unopenable_reparse_error(&error))
    {
        return LinkClass::Link;
    }
    class
}

#[cfg(not(windows))]
fn link_class(_path: &Path, metadata: &fs::Metadata) -> LinkClass {
    classify_link(metadata.file_type().is_symlink(), 0)
}

/// True for anything the tool will not read as ordinary data: a link, or a placeholder whose data
/// is elsewhere.
fn is_link_or_reparse(path: &Path, metadata: &fs::Metadata) -> bool {
    link_class(path, metadata) != LinkClass::Ordinary
}

/// The skip reason for an entry that is not ordinary data.
fn link_skip_reason(path: &Path, metadata: &fs::Metadata) -> Option<SkipReason> {
    match link_class(path, metadata) {
        LinkClass::Ordinary => None,
        LinkClass::Link => Some(SkipReason::LinkOrReparsePoint),
        LinkClass::DehydratedPlaceholder => Some(SkipReason::DehydratedPlaceholder),
    }
}

#[cfg(windows)]
fn has_unsupported_windows_attributes(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    // OFFLINE is classified with the other recall attributes by `classify_link`, which runs first.
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0004;
    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x0100;
    metadata.file_attributes() & (FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_TEMPORARY) != 0
}

#[cfg(not(windows))]
fn has_unsupported_windows_attributes(_metadata: &fs::Metadata) -> bool {
    false
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

    /// A link inside the tree is skipped and reported, never followed and never a whole-scan
    /// failure. It used to be the latter, which on Windows meant any OneDrive Files-On-Demand
    /// placeholder (a reparse point) stopped every file beside it from syncing.
    #[test]
    fn scan_skips_and_reports_a_link_within_the_source_tree() {
        let root = temp_dir("link-child");
        let outside = temp_dir("link-child-target");
        fs::write(outside.join("payload.txt"), b"payload").unwrap();
        fs::write(root.join("kept.txt"), b"kept").unwrap();
        let link = root.join("linked");
        if !try_make_link(&link, &outside) {
            eprintln!(
                "skipping scan_skips_and_reports_a_link_within_the_source_tree: could not create a link"
            );
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(outside).unwrap();
            return;
        }

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        assert_eq!(
            inventory.entries.keys().collect::<Vec<_>>(),
            ["kept.txt"],
            "the link is not followed and its sibling is still carried"
        );
        assert_eq!(
            inventory.skipped,
            [SkippedEntry {
                relative: "linked".to_owned(),
                reason: SkipReason::LinkOrReparsePoint,
            }]
        );
        assert_eq!(
            entry_at(&inventory.root, "linked")
                .unwrap()
                .map(|e| e.relative),
            None
        );

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn scan_skips_case_colliding_entries_on_case_sensitive_filesystems() {
        let root = temp_dir("case-collision-scan");
        fs::write(root.join("Config.txt"), b"a").unwrap();
        fs::write(root.join("config.txt"), b"b").unwrap();
        fs::write(root.join("other.txt"), b"c").unwrap();
        if fs::read_dir(&root).unwrap().count() < 3 {
            // The filesystem folded the two names together (the default on Windows and
            // macOS), so there is nothing left to collide. Skip on this platform.
            fs::remove_dir_all(root).unwrap();
            return;
        }

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        assert_eq!(inventory.entries.keys().collect::<Vec<_>>(), ["other.txt"]);
        assert_eq!(
            inventory
                .skipped
                .iter()
                .map(|skip| (skip.relative.as_str(), skip.reason.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Config.txt", "local-case-collision"),
                ("config.txt", "local-case-collision"),
            ]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_nonportable_drive_names_before_upload() {
        for relative in ["~scratch", "bad:name", "folder/NUL.txt", "trailing."] {
            assert!(drive_path_issue(relative).is_some(), "{relative}");
        }
        assert!(drive_path_issue("normal/fichier.txt").is_none());
    }

    fn output_of(paths: &[(&str, EntryKind)]) -> ScanOutput {
        let mut output = ScanOutput::default();
        for (relative, kind) in paths {
            output.entries.insert(
                (*relative).to_owned(),
                LocalEntry {
                    relative: (*relative).to_owned(),
                    full_path: PathBuf::from(relative),
                    kind: *kind,
                    size: 0,
                    mtime_ms: 0,
                    identity: FileIdentity::default(),
                    content_md5: None,
                },
            );
        }
        output
    }

    /// A colliding pair is dropped with everything beneath it and reported once per spelling, and
    /// nothing else in the tree is touched. Synthetic, so it runs on case-insensitive hosts too.
    #[test]
    fn a_portable_case_collision_drops_only_the_pair_and_its_subtrees() {
        let mut output = output_of(&[
            ("Folder", EntryKind::Directory),
            ("Folder/A.txt", EntryKind::File),
            ("folder", EntryKind::Directory),
            ("folder/a.TXT", EntryKind::File),
            ("folder-two", EntryKind::Directory),
            ("folder-two/b.txt", EntryKind::File),
            ("keep.txt", EntryKind::File),
        ]);
        skip_portable_case_collisions(&mut output);
        assert_eq!(
            output.entries.keys().collect::<Vec<_>>(),
            ["folder-two", "folder-two/b.txt", "keep.txt"]
        );
        let mut skipped = output.skipped.clone();
        skipped.sort_by(|left, right| left.relative.cmp(&right.relative));
        assert_eq!(
            skipped,
            [
                SkippedEntry {
                    relative: "Folder".to_owned(),
                    reason: SkipReason::LocalCaseCollision {
                        other: "folder".to_owned()
                    },
                },
                SkippedEntry {
                    relative: "folder".to_owned(),
                    reason: SkipReason::LocalCaseCollision {
                        other: "Folder".to_owned()
                    },
                },
            ]
        );
        assert!(skipped[0].reason.detail().contains("\"folder\""));

        let mut distinct = output_of(&[
            ("Folder/A.txt", EntryKind::File),
            ("Folder/B.txt", EntryKind::File),
        ]);
        skip_portable_case_collisions(&mut distinct);
        assert_eq!(distinct.entries.len(), 2);
        assert!(distinct.skipped.is_empty());
    }

    /// An NFC and an NFD spelling of one name are the same name to every Drive client, exactly as a
    /// case variant is.
    #[test]
    fn unicode_normalisation_variants_collide_like_case_variants() {
        let composed = "caf\u{e9}.txt";
        let decomposed = "cafe\u{301}.txt";
        let mut output = output_of(&[
            (composed, EntryKind::File),
            (decomposed, EntryKind::File),
            ("plain.txt", EntryKind::File),
        ]);
        skip_portable_case_collisions(&mut output);
        assert_eq!(output.entries.keys().collect::<Vec<_>>(), ["plain.txt"]);
        assert_eq!(output.skipped.len(), 2);
    }

    /// Built-in exclusions cover the OS metadata files that otherwise abort or pollute a run, in the
    /// case variants Windows produces, and stay overridable by an operator negation.
    #[test]
    fn default_excludes_cover_os_metadata_files_and_can_be_negated() {
        let root = temp_dir("default-excludes");
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        for name in [
            "desktop.ini",
            "Desktop.ini",
            "sub/DESKTOP.INI",
            "Thumbs.db",
            "thumbs.db",
            "ehthumbs.db",
            ".DS_Store",
            "deep/dir/.DS_Store",
            "Icon\r",
            "~$report.docx",
        ] {
            assert!(rules.is_ignored(name, false), "{name:?} should be excluded");
        }
        for name in [
            "Icon",
            "Iconr",
            "desktop.ini.bak",
            "my thumbs.db",
            "report.docx",
        ] {
            assert!(!rules.is_ignored(name, false), "{name:?} must stay payload");
        }

        let negated = IgnoreRules::build(&root, &["!desktop.ini".to_owned()]).unwrap();
        assert!(!negated.is_ignored("desktop.ini", false));
        assert!(negated.is_ignored("Thumbs.db", false));

        fs::remove_dir_all(root).unwrap();
    }

    /// `Icon\r` is a real macOS file name, and the control character in it used to fail the scan
    /// before any rule could apply. Excluded by default, it no longer reaches validation. Unix only:
    /// Windows refuses to create the name at all.
    #[cfg(unix)]
    #[test]
    fn a_macos_custom_icon_file_does_not_fail_the_scan() {
        let root = temp_dir("icon-cr");
        fs::write(root.join("Icon\r"), b"").unwrap();
        fs::write(root.join("photo.jpg"), b"jpg").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        assert_eq!(inventory.entries.keys().collect::<Vec<_>>(), ["photo.jpg"]);
        assert!(inventory.skipped.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    /// The deferring hash pass returns the one file that vanished and still hashes the rest.
    #[test]
    fn deferring_hashing_returns_failed_files_and_hashes_the_rest() {
        let root = temp_dir("deferring-hash");
        fs::write(root.join("gone.txt"), b"soon gone").unwrap();
        fs::write(root.join("kept.txt"), b"kept").unwrap();
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let mut inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        fs::remove_file(root.join("gone.txt")).unwrap();

        let failed = populate_content_md5_selective_deferring(
            &mut inventory,
            &BTreeSet::new(),
            &CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(
            failed
                .iter()
                .map(|(relative, _)| relative.as_str())
                .collect::<Vec<_>>(),
            ["gone.txt"]
        );
        assert!(inventory.entries["kept.txt"].content_md5.is_some());
        assert!(inventory.entries["gone.txt"].content_md5.is_none());

        inventory.withhold(
            "gone.txt",
            SkipReason::Deferred {
                detail: failed[0].1.to_string(),
            },
        );
        assert!(!inventory.entries.contains_key("gone.txt"));
        assert_eq!(inventory.skipped[0].reason.as_str(), "deferred");
        fs::remove_dir_all(root).unwrap();
    }

    /// review-core S3: only name surrogates are links, only recall attributes make a placeholder,
    /// and every other reparse point -- dedup, hydrated or pinned cloud files -- is ordinary data.
    #[test]
    fn reparse_points_are_classified_by_what_reading_them_would_do() {
        const REPARSE: u32 = 0x0000_0400;
        const ARCHIVE: u32 = 0x0000_0020;
        const PINNED: u32 = 0x0008_0000;
        const UNPINNED: u32 = 0x0010_0000;
        // Symbolic link, junction, mount point: name surrogates, whatever else is set.
        assert_eq!(classify_link(true, REPARSE), LinkClass::Link);
        assert_eq!(
            classify_link(true, REPARSE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS),
            LinkClass::Link
        );
        // Data Deduplication: a non-surrogate reparse point with its data on the volume.
        assert_eq!(classify_link(false, REPARSE | ARCHIVE), LinkClass::Ordinary);
        // OneDrive, hydrated or pinned ("always keep on this device").
        assert_eq!(classify_link(false, REPARSE | PINNED), LinkClass::Ordinary);
        assert_eq!(classify_link(false, REPARSE | ARCHIVE), LinkClass::Ordinary);
        // OneDrive online-only, and an HSM-offline file: reading would recall them.
        assert_eq!(
            classify_link(
                false,
                REPARSE | UNPINNED | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            ),
            LinkClass::DehydratedPlaceholder
        );
        assert_eq!(
            classify_link(false, REPARSE | FILE_ATTRIBUTE_RECALL_ON_OPEN),
            LinkClass::DehydratedPlaceholder
        );
        assert_eq!(
            classify_link(false, FILE_ATTRIBUTE_OFFLINE),
            LinkClass::DehydratedPlaceholder
        );
        // Plain files and directories.
        assert_eq!(classify_link(false, 0), LinkClass::Ordinary);
        assert_eq!(classify_link(false, ARCHIVE), LinkClass::Ordinary);
        assert_eq!(
            SkipReason::DehydratedPlaceholder.as_str(),
            "dehydrated-placeholder"
        );
    }

    /// The open-probe's decision: only 1920/1921 make a non-surrogate reparse point a link.
    #[cfg(windows)]
    #[test]
    fn only_the_unopenable_reparse_errors_reclassify_an_entry() {
        assert!(is_unopenable_reparse_error(
            &std::io::Error::from_raw_os_error(1920)
        ));
        assert!(is_unopenable_reparse_error(
            &std::io::Error::from_raw_os_error(1921)
        ));
        assert!(!is_unopenable_reparse_error(
            &std::io::Error::from_raw_os_error(5)
        ));
        assert!(!is_unopenable_reparse_error(
            &std::io::Error::from_raw_os_error(32)
        ));
        assert!(!is_unopenable_reparse_error(&std::io::Error::other(
            "no code"
        )));
    }

    /// A real app execution alias, where this machine has one: skipped as a link, never opened
    /// for hashing, and never a whole-run or partial-run failure.
    #[cfg(windows)]
    #[test]
    fn an_app_execution_alias_is_classified_as_a_link() {
        let Some(directory) = std::env::var_os("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("Microsoft").join("WindowsApps"))
        else {
            eprintln!("skipping: LOCALAPPDATA is not set");
            return;
        };
        let Ok(entries) = fs::read_dir(&directory) else {
            eprintln!("skipping: no WindowsApps directory");
            return;
        };
        let alias = entries.filter_map(std::result::Result::ok).find(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "exe")
                && fs::File::open(entry.path())
                    .err()
                    .is_some_and(|error| is_unopenable_reparse_error(&error))
        });
        let Some(alias) = alias else {
            eprintln!("skipping: no app execution alias on this machine");
            return;
        };
        let metadata = fs::symlink_metadata(alias.path()).unwrap();
        assert_eq!(link_class(&alias.path(), &metadata), LinkClass::Link);
        assert_eq!(
            link_skip_reason(&alias.path(), &metadata),
            Some(SkipReason::LinkOrReparsePoint)
        );
        eprintln!("checked app execution alias {}", alias.path().display());
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

    /// A name no encoding can represent, and the one the fixtures use.
    #[cfg(unix)]
    const NON_UTF8_NAME_BYTES: [u8; 3] = [b'b', 0xFF, b'b'];

    #[cfg(unix)]
    fn write_non_utf8_named_file(dir: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;

        let name = std::ffi::OsStr::from_bytes(&NON_UTF8_NAME_BYTES);
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

    /// An excluded name the tool refuses must not fail the run.
    ///
    /// The common case first: Word opens `report.docx` and creates `~$report.docx` beside it, and
    /// every scheduled run then failed with "names beginning with ~ are not synchronized" until the
    /// document was closed. `--exclude '~$*'` had no effect, because the name was validated before
    /// the rules were consulted. `aux.h` is the second: an ordinary name in C and in any tree of
    /// Unix origin, and one that Windows reserves.
    #[test]
    fn an_excluded_name_the_tool_refuses_does_not_fail_the_scan() {
        for (name, pattern) in [
            ("~$report.docx", "~$*"),
            ("aux.h", "aux.h"),
            ("trailing.", "trailing."),
            ("bad:name", "bad:name"),
        ] {
            let root = temp_dir("excluded-refused-name");
            if fs::write(root.join(name), b"payload").is_err() {
                // Windows will not create some of these names at all, which is the same outcome by
                // a different route: there is nothing to exclude.
                fs::remove_dir_all(&root).unwrap();
                continue;
            }
            fs::write(root.join("keep.txt"), b"payload").unwrap();
            let rules = IgnoreRules::build(&root, &[pattern.to_owned()]).unwrap();

            let inventory = scan(&root, &rules, &CancellationToken::default())
                .unwrap_or_else(|error| panic!("{name:?} excluded by {pattern:?}: {error}"));

            assert!(!inventory.entries.contains_key(name), "{name:?}");
            assert!(inventory.entries.contains_key("keep.txt"), "{name:?}");
            fs::remove_dir_all(&root).unwrap();
        }
    }

    /// A refused name that nothing excludes still fails the scan. The reorder must not weaken the
    /// refusal. (`~$report.docx`, the Office lock file, is a built-in exclusion now, so the name
    /// here is an ordinary `~` name.)
    #[test]
    fn an_unexcluded_name_the_tool_refuses_still_fails_the_scan() {
        let root = temp_dir("unexcluded-refused-name");
        if fs::write(root.join("~scratch.txt"), b"payload").is_err() {
            fs::remove_dir_all(&root).unwrap();
            return;
        }
        let rules = IgnoreRules::build(&root, &[]).unwrap();

        assert!(matches!(
            scan(&root, &rules, &CancellationToken::default()),
            Err(Error::UnsupportedLocalEntry { reason, .. }) if reason.contains("beginning with ~")
        ));

        fs::remove_dir_all(&root).unwrap();
    }

    /// A non-UTF-8 name the operator excluded is skipped; one they did not still fails.
    ///
    /// The rules are matched against the lossy spelling to decide exclusion only. A name that
    /// survives that cannot be mapped to a remote path at all, so refusing it stays right.
    #[cfg(unix)]
    #[test]
    fn an_excluded_non_utf8_name_is_skipped_rather_than_refused() {
        let root = temp_dir("non-utf8-excluded");
        if !write_non_utf8_named_file(&root) {
            eprintln!("skipping: filesystem rejected the name");
            fs::remove_dir_all(root).unwrap();
            return;
        }
        fs::write(root.join("keep.txt"), b"payload").unwrap();
        // The pattern is the lossy spelling itself, which is exactly what the rules are matched
        // against. Deriving it here rather than writing a glob that happens to cover it keeps the
        // test about the documented behaviour instead of about how globset treats U+FFFD.
        let lossy = String::from_utf8_lossy(&NON_UTF8_NAME_BYTES).into_owned();
        let rules = IgnoreRules::build(&root, &[lossy]).unwrap();

        let inventory = scan(&root, &rules, &CancellationToken::default())
            .expect("an excluded name that cannot be represented is not a reason to fail");

        assert_eq!(inventory.entries.len(), 1);
        assert!(inventory.entries.contains_key("keep.txt"));

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
    fn set_attributes(target: &Path, flags: &[&str]) -> bool {
        let status = std::process::Command::new("attrib")
            .args(flags)
            .arg(target)
            .status();
        matches!(status, Ok(status) if status.success())
    }

    /// A SYSTEM file is skipped and reported, its siblings still sync, and a later read of the same
    /// path agrees with the walk.
    #[cfg(windows)]
    #[test]
    fn scan_skips_and_reports_windows_system_attribute_files() {
        let root = temp_dir("windows-system-attribute");
        let target = root.join("system.dat");
        fs::write(&target, b"payload").unwrap();
        fs::write(root.join("report.docx"), b"doc").unwrap();
        if !set_attributes(&target, &["+S"]) {
            eprintln!(
                "skipping scan_skips_and_reports_windows_system_attribute_files: could not set the SYSTEM attribute"
            );
            fs::remove_dir_all(root).unwrap();
            return;
        }

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        assert_eq!(
            inventory.entries.keys().collect::<Vec<_>>(),
            ["report.docx"]
        );
        assert_eq!(
            inventory.skipped,
            [SkippedEntry {
                relative: "system.dat".to_owned(),
                reason: SkipReason::WindowsAttributes,
            }]
        );
        assert!(entry_at(&inventory.root, "system.dat").unwrap().is_none());

        let _ = set_attributes(&target, &["-S"]);
        fs::remove_dir_all(root).unwrap();
    }

    /// N-1, the shape every Windows profile has: `desktop.ini` is Hidden+System in Documents,
    /// Pictures, Music and Desktop. It used to abort every scan of those folders. Excluded by
    /// default it is neither carried nor reported; re-included by the operator, it is skipped and
    /// reported rather than refused.
    #[cfg(windows)]
    #[test]
    fn a_hidden_system_desktop_ini_neither_fails_nor_clutters_the_scan() {
        let root = temp_dir("desktop-ini");
        let documents = root.join("Documents");
        fs::create_dir(&documents).unwrap();
        let ini = documents.join("desktop.ini");
        fs::write(&ini, b"[.ShellClassInfo]").unwrap();
        fs::write(documents.join("report.docx"), b"doc").unwrap();
        if !set_attributes(&ini, &["+H", "+S"]) {
            eprintln!("skipping: could not set the HIDDEN and SYSTEM attributes");
            fs::remove_dir_all(root).unwrap();
            return;
        }

        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let inventory = scan(&root, &rules, &CancellationToken::default()).unwrap();
        assert_eq!(
            inventory.entries.keys().collect::<Vec<_>>(),
            ["Documents", "Documents/report.docx"]
        );
        assert!(inventory.skipped.is_empty());

        let reincluded = IgnoreRules::build(&root, &["!desktop.ini".to_owned()]).unwrap();
        let inventory = scan(&root, &reincluded, &CancellationToken::default()).unwrap();
        assert_eq!(
            inventory.entries.keys().collect::<Vec<_>>(),
            ["Documents", "Documents/report.docx"]
        );
        assert_eq!(
            inventory
                .skipped
                .iter()
                .map(|skip| (skip.relative.as_str(), skip.reason.as_str()))
                .collect::<Vec<_>>(),
            [("Documents/desktop.ini", "windows-attributes")]
        );

        let _ = set_attributes(&ini, &["-H", "-S"]);
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

    /// A tree of *exactly* the budget is complete, not truncated.
    ///
    /// The boundary, which is the case an off-by-one hides in: the budget is how many entries may
    /// be kept, so the walk is partial only once an entry has actually been refused. Publishing a
    /// full tree as incomplete is not cosmetic — `store_rollup` requires a reader to render
    /// incomplete totals as a floor, so every count in the dashboard became "20,000+" for a tree
    /// that was exactly 20,000.
    #[test]
    fn a_tree_of_exactly_the_budget_is_complete() {
        let root = tree("budget-boundary");
        let rules = IgnoreRules::build(&root, &[]).unwrap();
        let scope = Scope::root();
        let all = scan_scoped(
            &root,
            &rules,
            &scope,
            false,
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();
        let exact = all.inventory.entries.len();
        assert!(exact > 1, "the fixture must have entries to count");

        let at_budget = scan_scoped(
            &root,
            &rules,
            &scope,
            false,
            exact,
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(
            at_budget.complete,
            "a walk that kept every entry it found is not partial"
        );
        assert_eq!(
            at_budget.inventory.entries.keys().collect::<Vec<_>>(),
            all.inventory.entries.keys().collect::<Vec<_>>()
        );

        // One entry short of the tree is partial, and keeps exactly the budget.
        let under_budget = scan_scoped(
            &root,
            &rules,
            &scope,
            false,
            exact - 1,
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(!under_budget.complete);
        assert_eq!(under_budget.inventory.entries.len(), exact - 1);

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
