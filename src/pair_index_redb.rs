//! The verified-pair index on disk: one redb database per profile (perf-design §2.2, §2.7, §9).
//!
//! redb 4.4 reports a truncated, foreign or locked file as an error at open, so the file is
//! opened in this process. It still panics when a read meets a corrupted page that has not been
//! verified (perf-design measured 29 of 80 random corruptions), so the first open of each run is
//! an [`open_verified`]: `check_integrity()` verifies every page checksum (and repairs an
//! interrupted commit) before any table is read. A reopen later in the same run skips it only
//! when the file is exactly as this process left it (see [`Fingerprint`]).
//!
//! Damage of any kind retires the file: it is set aside as `<stem>.redb.corrupt-<epoch>` and
//! rebuilt from the run's own evidence, and nothing read from it is served. That covers a file
//! that fails the check, one redb had to repair (on the first open or a reopen), and a record,
//! key or `meta` value that does not decode. Every key and value is `&[u8]` and checked here, so
//! no stored byte can reach a decoder that unwraps.
//!
//! redb also panics inside `create_file`, before `check_integrity` can run, on some corruptions
//! of its saved allocator state (a single flipped byte, found by a byte-flip sweep). A release
//! build aborts on a panic, so such a file would crash every run. While a thread is inside redb
//! for an index file, a panic hook records the panic beside it (`<stem>.redb.panicked`); the next
//! open finds the record and retires the file instead of opening it, so one crashed run heals.
//!
//! The file is held in two short windows: W1 reads `meta` and the pairs the comparison set names,
//! then closes; W2, at the end of the run, reopens and writes one transaction. Between them the
//! file is unlocked for however long the run takes, so a dashboard read is never queued behind a
//! sync.
//!
//! Every way the index cannot be used is a named [`IndexIssue`] and a fully live run; none fails
//! a run or makes it partial.

use std::cell::RefCell;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{FileExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use redb::{
    Database, Durability, ReadOnlyTable, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};

use crate::pair_index::{
    Binding, IndexCommit, IndexIssue, IndexMeta, PairIndex, PairRecord, RemoteSignal, StoredPair,
};

/// The redb release the index is written by, recorded in `meta.writer`.
pub const REDB_VERSION: &str = "4.4.0";
/// The directory marker.
pub const MARKER: &str = ".sdsync-pair-index";
const MARKER_PREFIX: &str = "sdsync.pair-index.";
const SUFFIX_DATABASE: &str = ".redb";
const SUFFIX_CORRUPT: &str = ".redb.corrupt-";
const SUFFIX_PANICKED: &str = ".redb.panicked";
/// The bytes of the file a [`Fingerprint`] keeps: redb's header (magic, flags, both commit
/// slots with their transaction ids and checksums) is the first 320.
const HEADER_BYTES: u64 = 512;
/// The most of a panic message a `.panicked` record keeps.
const PANIC_RECORD_BYTES: usize = 300;
/// Default redb cache. Mandatory: redb's 1 GiB default reached +642 MB RSS at 1M entries.
pub const DEFAULT_CACHE_MIB: u64 = 8;
pub const MAX_CACHE_MIB: u64 = 64;
/// How long the core waits for another holder of the file (a dashboard read) to let go.
pub const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOCK_RETRY: Duration = Duration::from_millis(250);
/// Quarantined copies older than this are swept.
const QUARANTINE_AGE: Duration = Duration::from_secs(7 * 86_400);
/// Earlier copies of one profile that could not be removed, past which a damaged file is deleted
/// instead of set aside, so copies cannot pile up without bound.
const MAX_STUCK_COPIES: usize = 3;
/// Bytes per stored pair the compaction rule budgets.
const COMPACT_BYTES_PER_ENTRY: u64 = 300;
const COMPACT_SLACK_BYTES: u64 = 64 * 1024 * 1024;

const META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("meta");
const PAIRS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("pairs");
const RESPELL: TableDefinition<&[u8], &[u8]> = TableDefinition::new("respell");

/// The fixed little-endian record a pair is stored as.
pub const RECORD_BYTES: usize = 104;
const FLAG_RACY: u64 = 1;
const FLAG_HAS_RCTIME: u64 = 1 << 1;
const FLAG_HAS_RCRTIME: u64 = 1 << 2;
const FLAG_HAS_NAS_TIME: u64 = 1 << 3;
const FLAG_RESPELLED: u64 = 1 << 4;
const FLAGS_KNOWN: u64 =
    FLAG_RACY | FLAG_HAS_RCTIME | FLAG_HAS_RCRTIME | FLAG_HAS_NAS_TIME | FLAG_RESPELLED;

/// Encode `record` (and whether its remote spelling differs) as [`RECORD_BYTES`] bytes:
/// `size | lm ns | lc ns | dev | ino | rm | rc | rcr | md5[16] | t | tn | flags`.
pub fn encode_record(record: &PairRecord, respelled: bool) -> [u8; RECORD_BYTES] {
    let mut out = [0_u8; RECORD_BYTES];
    let mut flags = 0;
    if record.racy {
        flags |= FLAG_RACY;
    }
    if record.remote_ctime.is_some() {
        flags |= FLAG_HAS_RCTIME;
    }
    if record.remote_crtime.is_some() {
        flags |= FLAG_HAS_RCRTIME;
    }
    if record.verified_at_nas.is_some() {
        flags |= FLAG_HAS_NAS_TIME;
    }
    if respelled {
        flags |= FLAG_RESPELLED;
    }
    let words: [u64; 8] = [
        record.size,
        record.local_mtime_ns as u64,
        record.local_ctime_ns as u64,
        record.device,
        record.inode,
        record.remote_mtime as u64,
        record.remote_ctime.unwrap_or(0) as u64,
        record.remote_crtime.unwrap_or(0) as u64,
    ];
    for (index, word) in words.iter().enumerate() {
        out[index * 8..index * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }
    out[64..80].copy_from_slice(&record.md5);
    out[80..88].copy_from_slice(&record.verified_at.to_le_bytes());
    out[88..96].copy_from_slice(&record.verified_at_nas.unwrap_or(0).to_le_bytes());
    out[96..104].copy_from_slice(&flags.to_le_bytes());
    out
}

/// Decode a stored record, or `None` when it is not exactly one.
pub fn decode_record(bytes: &[u8]) -> Option<(PairRecord, bool)> {
    if bytes.len() != RECORD_BYTES {
        return None;
    }
    let word = |offset: usize| {
        let mut buffer = [0_u8; 8];
        buffer.copy_from_slice(&bytes[offset..offset + 8]);
        u64::from_le_bytes(buffer)
    };
    let flags = word(96);
    if flags & !FLAGS_KNOWN != 0 {
        return None;
    }
    let mut md5 = [0_u8; 16];
    md5.copy_from_slice(&bytes[64..80]);
    let record = PairRecord {
        size: word(0),
        local_mtime_ns: word(8) as i64,
        local_ctime_ns: word(16) as i64,
        device: word(24),
        inode: word(32),
        remote_mtime: word(40) as i64,
        remote_ctime: (flags & FLAG_HAS_RCTIME != 0).then_some(word(48) as i64),
        remote_crtime: (flags & FLAG_HAS_RCRTIME != 0).then_some(word(56) as i64),
        md5,
        verified_at: word(80) as i64,
        verified_at_nas: (flags & FLAG_HAS_NAS_TIME != 0).then_some(word(88) as i64),
        racy: flags & FLAG_RACY != 0,
    };
    Some((record, flags & FLAG_RESPELLED != 0))
}

/// The database file for `profile` in `directory`.
#[derive(Clone, Debug)]
pub struct IndexPaths {
    pub directory: PathBuf,
    pub profile: String,
    pub database: PathBuf,
    /// Where a panic inside redb on this file is recorded (see the module documentation).
    pub panicked: PathBuf,
}

impl IndexPaths {
    pub fn new(directory: &Path, profile: &str) -> Result<Self, IndexIssue> {
        if !crate::evidence_dir::name_is_safe(profile) {
            return Err(IndexIssue::warning(
                "pair-index-unsafe",
                directory.display().to_string(),
                format!("profile name {profile:?} cannot name an index file"),
            ));
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            profile: profile.to_owned(),
            database: directory.join(format!("{profile}{SUFFIX_DATABASE}")),
            panicked: directory.join(format!("{profile}{SUFFIX_PANICKED}")),
        })
    }

    pub fn label(&self) -> String {
        self.database.display().to_string()
    }
}

/// Whether the index could have written `name` in its directory.
fn is_index_owned(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name == MARKER {
        return true;
    }
    if let Some((profile, epoch)) = name.split_once(SUFFIX_CORRUPT) {
        return crate::evidence_dir::name_is_safe(profile)
            && !epoch.is_empty()
            && epoch.bytes().all(|byte| byte.is_ascii_digit());
    }
    if let Some(profile) = name.strip_suffix(SUFFIX_PANICKED) {
        return crate::evidence_dir::name_is_safe(profile);
    }
    name.strip_suffix(SUFFIX_DATABASE)
        .is_some_and(crate::evidence_dir::name_is_safe)
}

thread_local! {
    /// The `.panicked` record of the index file this thread is inside redb for, if any.
    static INSIDE_REDB: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// While alive, a panic on this thread is recorded beside the index file it is working on, so
/// the next open retires a file redb cannot read without crashing (see the module documentation).
struct InsideRedb {
    previous: Option<PathBuf>,
}

impl InsideRedb {
    fn enter(paths: &IndexPaths) -> Self {
        install_panic_hook();
        let previous = INSIDE_REDB.with(|cell| cell.replace(Some(paths.panicked.clone())));
        Self { previous }
    }
}

impl Drop for InsideRedb {
    fn drop(&mut self) {
        let previous = self.previous.take();
        let _ = INSIDE_REDB.try_with(|cell| *cell.borrow_mut() = previous);
    }
}

/// Chain a hook in front of whatever hook is installed. It acts only on a thread inside redb for
/// an index file; every panic still reaches the previous hook (and then aborts, in a release).
fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let record = INSIDE_REDB
                .try_with(|cell| cell.try_borrow().ok().and_then(|path| path.clone()))
                .ok()
                .flatten();
            if let Some(path) = record {
                let message: String = info
                    .to_string()
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .take(PANIC_RECORD_BYTES)
                    .collect();
                if let Err(error) = write_panic_record(&path, &message) {
                    // The process is going down; standard error is the only place left.
                    eprintln!(
                        "warning: pair-index-corrupt: redb panicked on the index and the panic \
                         could not be recorded at {} ({error}); delete the index file by hand if \
                         every run stops here",
                        path.display()
                    );
                }
            }
            previous(info);
        }));
    });
}

fn write_panic_record(path: &Path, message: &str) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    #[cfg(target_os = "linux")]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path)?;
    file.write_all(message.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// The panic a previous process recorded inside redb on this file, if any.
fn panic_record(paths: &IndexPaths) -> Option<String> {
    let metadata = fs::symlink_metadata(&paths.panicked).ok()?;
    let text = if metadata.is_file() {
        fs::read(&paths.panicked)
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
            .unwrap_or_else(|error| format!("the record could not be read: {error}"))
    } else {
        "the record is not a regular file".to_owned()
    };
    let text: String = text.chars().take(PANIC_RECORD_BYTES).collect();
    Some(format!(
        "a previous run stopped inside redb on this file ({text}), which a damaged page \
         causes; it is not opened again"
    ))
}

/// What this process knows of the file as it last closed it: identity, length, times and
/// redb's header. Every redb commit rewrites the header's transaction id and slot checksums,
/// and every writable open and close commits or rewrites it, so another writer cannot leave
/// all of these as they were.
///
/// A reopen within the same run that finds exactly this skips `check_integrity()`: the pages
/// were verified by this run's first open and written since only by this process through redb.
/// The first open of each run always verifies (redb can panic on an unverified corrupt page),
/// and any difference at all verifies again. What this does not see is a change between the
/// fingerprint read and redb taking its lock (microseconds), or one that restores every byte of
/// the header and every time to the nanosecond; the next run's first open verifies either way.
/// It saves a full read of the file per reopen (three or four per run), which on an armv7 NAS
/// is most of the index's cost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fingerprint {
    device: u64,
    inode: u64,
    length: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    header: Vec<u8>,
}

impl Fingerprint {
    fn of(file: &fs::File) -> Option<Self> {
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        let mut header = vec![0_u8; usize::try_from(HEADER_BYTES.min(metadata.len())).ok()?];
        file.read_exact_at(&mut header, 0).ok()?;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
            header,
        })
    }

    fn of_path(path: &Path) -> Option<Self> {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(target_os = "linux")]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        Self::of(&options.open(path).ok()?)
    }
}

/// Make `directory` the index's own: created 0700 when missing, private, and claimed by marker.
///
/// `Ok` carries what the directory's upkeep could not do (a quarantined copy that could not be
/// swept), for the run to name; none of it stops the index being used.
pub fn prepare_directory(directory: &Path) -> Result<Vec<IndexIssue>, IndexIssue> {
    let unusable = |cause: String| {
        IndexIssue::warning(
            "pair-index-directory-unusable",
            directory.display().to_string(),
            cause,
        )
    };
    match fs::symlink_metadata(directory) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt as _;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .map_err(|error| unusable(format!("could not be created: {error}")))?;
        }
        Err(error) => return Err(unusable(format!("could not be examined: {error}"))),
    }
    crate::evidence_dir::directory_is_private(directory).map_err(unusable)?;
    crate::evidence_dir::claim_directory(
        directory,
        &crate::evidence_dir::DirectorySpec {
            marker: MARKER,
            marker_prefix: MARKER_PREFIX,
            marker_line: crate::pair_index::SCHEMA,
            kind: "pair-index",
            flag: "--pair-index",
            subject: "the index",
            this: "this index",
            owns: &is_index_owned,
        },
    )
    .map_err(unusable)?;
    Ok(sweep_quarantine(directory).into_iter().collect())
}

/// Remove quarantined copies past [`QUARANTINE_AGE`]. A copy that stays costs only space, so a
/// failure does not stop the run; it is named once (`pair-index-cleanup-failed`, the first path
/// and cause, and how many more), because copies that can never be removed would pile up.
fn sweep_quarantine(directory: &Path) -> Option<IndexIssue> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            return Some(IndexIssue::warning(
                "pair-index-cleanup-failed",
                directory.display().to_string(),
                format!("quarantined copies could not be listed for their weekly sweep: {error}"),
            ));
        }
    };
    let now = SystemTime::now();
    let mut failures = Failures::default();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failures.add(directory, format!("an entry could not be read: {error}"));
                continue;
            }
        };
        let is_copy = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.contains(SUFFIX_CORRUPT));
        if !is_copy {
            continue;
        }
        // Not following a link: a link named like a copy is the link's to lose, not its target's.
        let old = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > QUARANTINE_AGE);
        if old && let Err(error) = fs::remove_file(entry.path()) {
            failures.add(&entry.path(), error.to_string());
        }
    }
    failures.issue("a quarantined copy older than 7 days could not be removed")
}

/// Removals that failed, named once: the first path and cause, and how many more.
#[derive(Default)]
struct Failures {
    first: Option<(PathBuf, String)>,
    count: usize,
}

impl Failures {
    fn add(&mut self, path: &Path, cause: String) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some((path.to_path_buf(), cause));
        }
    }

    fn issue(self, what: &str) -> Option<IndexIssue> {
        let (path, cause) = self.first?;
        let more = match self.count {
            1 => String::new(),
            count => format!(" ({} more failed the same way or another)", count - 1),
        };
        Some(IndexIssue::warning(
            "pair-index-cleanup-failed",
            path.display().to_string(),
            format!("{what}: {cause}{more}"),
        ))
    }
}

/// Open the database file the safe way: never through a link, created 0600, and only a regular
/// file owned by its directory's owner with no group or other bits.
fn open_file_safely(paths: &IndexPaths) -> Result<fs::File, IndexIssue> {
    let unsafe_file =
        |cause: String| IndexIssue::warning("pair-index-unsafe", paths.label(), cause);
    let directory_owner = fs::symlink_metadata(&paths.directory)
        .map_err(|error| unsafe_file(format!("its directory could not be examined: {error}")))?
        .uid();
    match fs::symlink_metadata(&paths.database) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(unsafe_file("is a symbolic link".to_owned()));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(unsafe_file("is not a regular file".to_owned()));
        }
        _ => {}
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    // Never through a link, even one planted between the check above and this open. Elsewhere
    // the check above and the owner and type checks below stand in, in a 0700 directory.
    #[cfg(target_os = "linux")]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(&paths.database).map_err(|error| {
        let code = match error.kind() {
            std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied => {
                "pair-index-readonly"
            }
            _ => "pair-index-unsafe",
        };
        IndexIssue::warning(code, paths.label(), format!("could not be opened: {error}"))
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| unsafe_file(format!("could not be examined once open: {error}")))?;
    if !metadata.is_file() {
        return Err(unsafe_file("is not a regular file".to_owned()));
    }
    if metadata.uid() != directory_owner {
        return Err(unsafe_file(format!(
            "is owned by uid {}, not the directory's owner {directory_owner}",
            metadata.uid()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(unsafe_file(format!(
            "has mode {:o}; the index must not be readable or writable by group or other",
            metadata.mode() & 0o777
        )));
    }
    Ok(file)
}

fn builder(cache_mib: u64) -> redb::Builder {
    let mut builder = redb::Builder::new();
    let bytes = cache_mib.clamp(1, MAX_CACHE_MIB) * 1024 * 1024;
    builder.set_cache_size(usize::try_from(bytes).unwrap_or(8 << 20));
    builder
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Set a file that failed its check aside as `<profile>.redb.corrupt-<epoch>`, keeping one copy,
/// so the run rebuilds from its own evidence.
fn quarantine(paths: &IndexPaths) -> Result<Quarantined, String> {
    let prefix = format!("{}{SUFFIX_CORRUPT}", paths.profile);
    let mut failures = Failures::default();
    match fs::read_dir(&paths.directory) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry)
                        if entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.starts_with(&prefix)) =>
                    {
                        if let Err(error) = fs::remove_file(entry.path()) {
                            failures.add(&entry.path(), error.to_string());
                        }
                    }
                    Ok(_) => {}
                    Err(error) => failures.add(
                        &paths.directory,
                        format!("an entry could not be read: {error}"),
                    ),
                }
            }
        }
        Err(error) => failures.add(
            &paths.directory,
            format!("earlier copies could not be listed: {error}"),
        ),
    }
    let stuck = failures.count;
    let cleanup = failures.issue("an earlier quarantined copy could not be removed");
    let kept = if stuck >= MAX_STUCK_COPIES {
        // Copies that cannot be removed would pile up, one per damaged file: past the bound the
        // damaged file is deleted instead of kept, and the cleanup failure says why.
        fs::remove_file(&paths.database).map_err(|error| {
            format!(
                "could not be removed ({error}), and is not set aside because {stuck} earlier \
                 copies could not be removed either"
            )
        })?;
        None
    } else {
        let target =
            paths
                .directory
                .join(format!("{}{SUFFIX_CORRUPT}{}", paths.profile, unix_now()));
        fs::rename(&paths.database, &target)
            .map_err(|error| format!("could not be set aside as {}: {error}", target.display()))?;
        Some(target)
    };
    // The panic record names the file just set aside, not the one that replaces it.
    match fs::remove_file(&paths.panicked) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "was set aside, but its panic record {} could not be removed ({error}), so the \
                 next run would set the new file aside too",
                paths.panicked.display()
            ));
        }
    }
    Ok(Quarantined { kept, cleanup })
}

/// What setting a damaged file aside did.
struct Quarantined {
    /// Where the copy was kept, or `None` when it was deleted (see [`MAX_STUCK_COPIES`]).
    kept: Option<PathBuf>,
    /// Earlier copies that could not be removed, named once.
    cleanup: Option<IndexIssue>,
}

impl Quarantined {
    fn describe(&self) -> String {
        match &self.kept {
            Some(target) => format!("set aside as {}", target.display()),
            None => format!(
                "deleted rather than set aside, because {MAX_STUCK_COPIES} or more earlier \
                 copies could not be removed"
            ),
        }
    }
}

/// What opening and checking the file found.
#[derive(Debug, Eq, PartialEq)]
pub enum CheckVerdict {
    /// Every page verified; `repaired` when redb had to repair damage.
    Passed { repaired: bool },
    /// The file is damaged, or not a redb file.
    Corrupt(String),
    /// Written in a format this redb does not read.
    Unsupported(String),
    /// Another process held it for [`LOCK_WAIT`].
    Locked,
    /// The file or its volume does not let this process write.
    ReadOnly(String),
    /// The file or its directory failed the safety checks.
    Unsafe(IndexIssue),
}

/// Open the file (waiting for another holder) and verify it before anything reads it.
///
/// redb 4.4 can panic reading a page that is corrupted and unverified, so no table is opened on
/// a file `check_integrity()` has not passed in this open.
pub fn open_verified(paths: &IndexPaths, cache_mib: u64) -> (Option<Database>, CheckVerdict) {
    let opened = open_checked(paths, cache_mib, None);
    (opened.database, opened.verdict)
}

/// What [`open_checked`] produced.
struct Checked {
    database: Option<Database>,
    verdict: CheckVerdict,
    /// `check_integrity()` ran (whatever it found).
    verified: bool,
}

/// Open the file, verified unless it is exactly `trusted` (see [`Fingerprint`]). A file with a
/// panic record is not opened at all: it is reported corrupt, to be set aside.
fn open_checked(paths: &IndexPaths, cache_mib: u64, trusted: Option<&Fingerprint>) -> Checked {
    let unopened = |verdict| Checked {
        database: None,
        verdict,
        verified: false,
    };
    if let Some(cause) = panic_record(paths) {
        if fs::symlink_metadata(&paths.database).is_ok() {
            return unopened(CheckVerdict::Corrupt(cause));
        }
        // The file it names is gone already; the record has nothing left to protect.
        if let Err(error) = fs::remove_file(&paths.panicked) {
            return unopened(CheckVerdict::Unsafe(IndexIssue::warning(
                "pair-index-cleanup-failed",
                paths.panicked.display().to_string(),
                format!("a panic record for a file that is gone could not be removed: {error}"),
            )));
        }
    }
    let _inside = InsideRedb::enter(paths);
    let (mut database, found) = match open_database(paths, cache_mib) {
        Ok(opened) => opened,
        Err(Opening::Issue(issue)) => return unopened(CheckVerdict::Unsafe(issue)),
        Err(Opening::Redb(error)) => return unopened(verdict_for(error)),
    };
    if trusted.is_some() && found.as_ref() == trusted {
        return Checked {
            database: Some(database),
            verdict: CheckVerdict::Passed { repaired: false },
            verified: false,
        };
    }
    match database.check_integrity() {
        Ok(clean) => Checked {
            database: Some(database),
            verdict: CheckVerdict::Passed { repaired: !clean },
            verified: true,
        },
        Err(error) => Checked {
            database: None,
            verdict: verdict_for(error.into()),
            verified: true,
        },
    }
}

fn verdict_for(error: redb::Error) -> CheckVerdict {
    match error {
        redb::Error::DatabaseAlreadyOpen => CheckVerdict::Locked,
        redb::Error::UpgradeRequired(format) => CheckVerdict::Unsupported(format!(
            "file format {format} is not one redb {REDB_VERSION} reads"
        )),
        redb::Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            CheckVerdict::ReadOnly(error.to_string())
        }
        redb::Error::Io(error) if error.kind() == std::io::ErrorKind::StorageFull => {
            CheckVerdict::ReadOnly(format!("the volume is full: {error}"))
        }
        other => CheckVerdict::Corrupt(other.to_string()),
    }
}

enum Opening {
    Issue(IndexIssue),
    Redb(redb::Error),
}

/// Open the database, waiting up to [`LOCK_WAIT`] for another holder, with the fingerprint the
/// file had just before redb opened it.
fn open_database(
    paths: &IndexPaths,
    cache_mib: u64,
) -> Result<(Database, Option<Fingerprint>), Opening> {
    let started = Instant::now();
    loop {
        let file = open_file_safely(paths).map_err(Opening::Issue)?;
        let found = Fingerprint::of(&file);
        match builder(cache_mib).create_file(file) {
            Ok(database) => return Ok((database, found)),
            Err(redb::DatabaseError::DatabaseAlreadyOpen) if started.elapsed() < LOCK_WAIT => {
                std::thread::sleep(LOCK_RETRY);
            }
            Err(error) => return Err(Opening::Redb(error.into())),
        }
    }
}

/// The issue a redb error is reported as.
fn issue_for(paths: &IndexPaths, error: redb::Error, writing: bool) -> IndexIssue {
    let label = paths.label();
    match error {
        redb::Error::DatabaseAlreadyOpen => IndexIssue::warning(
            "pair-index-locked",
            label,
            format!(
                "another process held the index for more than {} s",
                LOCK_WAIT.as_secs()
            ),
        ),
        redb::Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
            ) =>
        {
            IndexIssue::warning(
                "pair-index-store-failed",
                label,
                format!("the volume is full: {error}"),
            )
        }
        redb::Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            IndexIssue::warning("pair-index-readonly", label, error.to_string())
        }
        other if writing => {
            IndexIssue::warning("pair-index-store-failed", label, other.to_string())
        }
        other => IndexIssue::warning("pair-index-corrupt", label, other.to_string()),
    }
}

/// The verified-pair index in one redb file.
pub struct RedbPairIndex {
    paths: IndexPaths,
    cache_mib: u64,
    database: Option<Database>,
    pairs: Option<ReadOnlyTable<&'static [u8], &'static [u8]>>,
    respell: Option<ReadOnlyTable<&'static [u8], &'static [u8]>>,
    meta: Option<Option<IndexMeta>>,
    notes: Vec<IndexIssue>,
    /// The file as this process last closed it, for a reopen to skip verification.
    trusted: Option<Fingerprint>,
    /// How many times `check_integrity()` ran on this run's opens.
    integrity_checks: usize,
    /// The stored evidence was set aside during the run; the session has not been told yet.
    discarded: bool,
    /// Pairs the last commit left, so the count needs no reopen.
    entries: Option<u64>,
}

/// What opening the index for a run produced: a backend, or none, and the issues met on the way.
pub struct Opened {
    pub backend: Option<RedbPairIndex>,
    pub issues: Vec<IndexIssue>,
}

/// What a damaged file is reported as.
#[derive(Clone, Copy)]
enum Damage {
    Corrupt,
    Unsupported,
}

impl Damage {
    fn code(self) -> &'static str {
        match self {
            Self::Corrupt => "pair-index-corrupt",
            Self::Unsupported => "pair-index-format-unsupported",
        }
    }
}

impl RedbPairIndex {
    /// Open the index, verified. A damaged, repaired or foreign-format file is set aside and
    /// replaced by an empty one; locked, read-only and unsafe files give no backend at all. Every
    /// case but a clean pass is named.
    pub fn open(paths: IndexPaths, cache_mib: u64) -> Opened {
        let mut backend = Self {
            paths,
            cache_mib,
            database: None,
            pairs: None,
            respell: None,
            meta: None,
            notes: Vec::new(),
            trusted: None,
            integrity_checks: 0,
            discarded: false,
            entries: None,
        };
        let opened = backend.ensure_open();
        let mut issues = std::mem::take(&mut backend.notes);
        match opened {
            Ok(()) => Opened {
                backend: Some(backend),
                issues,
            },
            Err(issue) => {
                issues.push(issue);
                Opened {
                    backend: None,
                    issues,
                }
            }
        }
    }

    /// How many times `check_integrity()` ran on this backend's opens: once per run, unless a
    /// reopen found the file changed.
    pub fn integrity_checks(&self) -> usize {
        self.integrity_checks
    }

    /// Open the file for a window: the run's first open is verified, a reopen of the file exactly
    /// as this process left it is not (see [`Fingerprint`]). Damage of any kind retires it.
    fn ensure_open(&mut self) -> Result<(), IndexIssue> {
        if self.database.is_some() {
            return Ok(());
        }
        let reopen = self.trusted.is_some();
        let opened = open_checked(&self.paths, self.cache_mib, self.trusted.as_ref());
        self.integrity_checks += usize::from(opened.verified);
        self.settle(opened.database, opened.verdict, reopen)
    }

    /// Take what an open found: keep a clean database, retire a damaged or repaired file, and
    /// refuse the rest by name.
    fn settle(
        &mut self,
        database: Option<Database>,
        verdict: CheckVerdict,
        reopen: bool,
    ) -> Result<(), IndexIssue> {
        match verdict {
            CheckVerdict::Passed { repaired: false } => {
                self.database = database;
                Ok(())
            }
            CheckVerdict::Passed { repaired: true } => {
                drop(database);
                // Repaired damage may have been anything: none of the evidence is trusted, and
                // it must not be trusted by a later run either, so the file goes, not just its
                // pairs.
                self.retire(
                    Damage::Corrupt,
                    if reopen {
                        "redb found and repaired damage in the file while the run worked"
                    } else {
                        "redb found and repaired damage in the file"
                    }
                    .to_owned(),
                )
            }
            CheckVerdict::Corrupt(cause) => self.retire(Damage::Corrupt, cause),
            CheckVerdict::Unsupported(cause) => self.retire(Damage::Unsupported, cause),
            CheckVerdict::Locked => Err(issue_for(
                &self.paths,
                redb::Error::DatabaseAlreadyOpen,
                false,
            )),
            CheckVerdict::ReadOnly(cause) => Err(IndexIssue::warning(
                "pair-index-readonly",
                self.paths.label(),
                cause,
            )),
            CheckVerdict::Unsafe(issue) => Err(issue),
        }
    }

    /// Set the file aside and continue on a new, empty, verified one: nothing read from the old
    /// one is served (the session is told through [`PairIndex::take_discarded`]), and the run's
    /// commit rebuilds the index from its own live evidence. Named with what was found.
    fn retire(&mut self, damage: Damage, cause: String) -> Result<(), IndexIssue> {
        self.pairs = None;
        self.respell = None;
        self.database = None;
        self.trusted = None;
        self.entries = None;
        let label = self.paths.label();
        let quarantined = quarantine(&self.paths).map_err(|why| {
            IndexIssue::warning(damage.code(), label.clone(), format!("{cause}; {why}"))
        })?;
        self.notes.push(IndexIssue::warning(
            damage.code(),
            label.clone(),
            format!(
                "{cause}; {}: nothing from it is served, and the index is rebuilt from this \
                 run's evidence",
                quarantined.describe()
            ),
        ));
        self.notes.extend(quarantined.cleanup);
        self.discarded = true;
        self.meta = Some(None);
        let fresh = open_checked(&self.paths, self.cache_mib, None);
        self.integrity_checks += usize::from(fresh.verified);
        match fresh.verdict {
            CheckVerdict::Passed { .. } => {
                self.database = fresh.database;
                Ok(())
            }
            other => Err(IndexIssue::warning(
                "pair-index-store-failed",
                label,
                format!("a new index could not be created: {other:?}"),
            )),
        }
    }

    /// Open the read transaction's tables, once per read window.
    fn read_tables(&mut self) -> Result<(), IndexIssue> {
        if self.pairs.is_some() {
            return Ok(());
        }
        self.ensure_open()?;
        let _inside = InsideRedb::enter(&self.paths);
        let database = self.database.as_ref().expect("opened above");
        let read = database
            .begin_read()
            .map_err(|error| issue_for(&self.paths, error.into(), false))?;
        self.pairs = match read.open_table(PAIRS) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(error) => return Err(issue_for(&self.paths, error.into(), false)),
        };
        self.respell = match read.open_table(RESPELL) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(error) => return Err(issue_for(&self.paths, error.into(), false)),
        };
        Ok(())
    }

    /// `meta`, or `Err(Ok(cause))` for a value that does not decode (the file is retired).
    fn read_meta_values(&mut self) -> Result<Option<IndexMeta>, Result<String, IndexIssue>> {
        self.ensure_open().map_err(Err)?;
        let _inside = InsideRedb::enter(&self.paths);
        let database = self.database.as_ref().expect("opened above");
        let read = database
            .begin_read()
            .map_err(|error| Err(issue_for(&self.paths, error.into(), false)))?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(Err(issue_for(&self.paths, error.into(), false))),
        };
        let get = |key: &str| -> Result<Option<String>, Result<String, IndexIssue>> {
            let Some(value) = table
                .get(key.as_bytes())
                .map_err(|error| Err(issue_for(&self.paths, error.into(), false)))?
            else {
                return Ok(None);
            };
            String::from_utf8(value.value().to_vec())
                .map(Some)
                .map_err(|_| Ok(format!("meta {key:?} is not UTF-8")))
        };
        let Some(schema) = get("schema")? else {
            return Ok(None);
        };
        let number = |value: Option<String>| value.and_then(|value| value.parse::<i64>().ok());
        Ok(Some(IndexMeta {
            schema,
            semantics_epoch: number(get("semantics_epoch")?)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0),
            binding: Binding {
                profile: get("profile")?.unwrap_or_default(),
                source: get("source")?.unwrap_or_default(),
                source_device: number(get("source_dev")?)
                    .and_then(|value| u64::try_from(value).ok())
                    .unwrap_or(0),
                remote: get("remote")?.unwrap_or_default(),
                server: get("server")?.unwrap_or_default(),
            },
            remote_signal: get("remote_change_signal")?
                .as_deref()
                .and_then(RemoteSignal::parse)
                .unwrap_or(RemoteSignal::LocalOnly),
            canary_at: number(get("canary_at")?).unwrap_or(0),
        }))
    }

    /// Close the file: the read window is over. What it looks like now is what a reopen in this
    /// run may trust.
    fn close(&mut self) {
        let was_open = self.database.is_some();
        self.pairs = None;
        self.respell = None;
        self.database = None;
        if was_open {
            self.trusted = Fingerprint::of_path(&self.paths.database);
        }
    }

    fn file_len(&self) -> u64 {
        fs::metadata(&self.paths.database).map_or(0, |metadata| metadata.len())
    }
}

/// How a stored pair read back: one, none, or a value that does not decode.
enum Decoded {
    Pair(StoredPair),
    Absent,
    Damaged(String),
}

impl PairIndex for RedbPairIndex {
    fn meta(&mut self) -> Result<Option<IndexMeta>, IndexIssue> {
        if let Some(meta) = &self.meta {
            return Ok(meta.clone());
        }
        let meta = match self.read_meta_values() {
            Ok(meta) => meta,
            Err(Ok(cause)) => {
                self.retire(Damage::Corrupt, cause)?;
                None
            }
            Err(Err(issue)) => return Err(issue),
        };
        self.meta = Some(meta.clone());
        Ok(meta)
    }

    fn get(&mut self, local_relative: &str) -> Result<Option<StoredPair>, IndexIssue> {
        self.read_tables()?;
        let decoded = {
            let _inside = InsideRedb::enter(&self.paths);
            let Some(pairs) = &self.pairs else {
                return Ok(None);
            };
            match pairs
                .get(local_relative.as_bytes())
                .map_err(|error| issue_for(&self.paths, error.into(), false))?
            {
                None => Decoded::Absent,
                Some(bytes) => match decode_record(bytes.value()) {
                    None => Decoded::Damaged(format!(
                        "the record for {local_relative:?} is not a valid pair record"
                    )),
                    Some((record, false)) => Decoded::Pair(StoredPair {
                        record,
                        remote_spelling: None,
                    }),
                    Some((record, true)) => {
                        let spelling = match &self.respell {
                            Some(table) => table
                                .get(local_relative.as_bytes())
                                .map_err(|error| issue_for(&self.paths, error.into(), false))?
                                .and_then(|value| String::from_utf8(value.value().to_vec()).ok()),
                            None => None,
                        };
                        match spelling {
                            Some(spelling) => Decoded::Pair(StoredPair {
                                record,
                                remote_spelling: Some(spelling),
                            }),
                            None => Decoded::Damaged(format!(
                                "the record for {local_relative:?} names a spelling that is \
                                 missing"
                            )),
                        }
                    }
                },
            }
        };
        match decoded {
            Decoded::Pair(pair) => Ok(Some(pair)),
            Decoded::Absent => Ok(None),
            Decoded::Damaged(cause) => {
                // One record that does not decode means the file holds bytes this build never
                // wrote: none of it is trusted, and it must not stop every later run either.
                self.retire(Damage::Corrupt, cause)?;
                Ok(None)
            }
        }
    }

    fn for_each_key(&mut self, visit: &mut dyn FnMut(&str)) -> Result<(), IndexIssue> {
        self.read_tables()?;
        let damaged = {
            let _inside = InsideRedb::enter(&self.paths);
            let Some(pairs) = &self.pairs else {
                return Ok(());
            };
            let range = pairs
                .iter()
                .map_err(|error| issue_for(&self.paths, error.into(), false))?;
            let mut damaged = false;
            for entry in range {
                // Stop at the first error rather than skip what could not be read.
                let (key, _) =
                    entry.map_err(|error| issue_for(&self.paths, error.into(), false))?;
                match std::str::from_utf8(key.value()) {
                    Ok(key) => visit(key),
                    Err(_) => {
                        damaged = true;
                        break;
                    }
                }
            }
            damaged
        };
        if damaged {
            self.retire(Damage::Corrupt, "a stored path is not UTF-8".to_owned())?;
        }
        Ok(())
    }

    fn finish_reads(&mut self) {
        self.close();
    }

    fn commit(&mut self, commit: IndexCommit) -> Result<(), IndexIssue> {
        self.pairs = None;
        self.respell = None;
        self.ensure_open()?;
        let _inside = InsideRedb::enter(&self.paths);
        let paths = self.paths.clone();
        let failed = |error: redb::Error| issue_for(&paths, error, true);
        let database = self.database.as_ref().expect("opened above");
        let mut write = database
            .begin_write()
            .map_err(|error| failed(error.into()))?;
        write
            .set_durability(Durability::Immediate)
            .map_err(|error| failed(error.into()))?;
        let applied = (|| -> Result<u64, redb::Error> {
            let mut pairs = write.open_table(PAIRS)?;
            let mut respell = write.open_table(RESPELL)?;
            if commit.clear {
                pairs.retain(|_, _| false)?;
                respell.retain(|_, _| false)?;
            }
            for key in &commit.remove {
                pairs.remove(key.as_bytes())?;
                respell.remove(key.as_bytes())?;
            }
            for (key, stored) in &commit.upsert {
                let bytes = encode_record(&stored.record, stored.remote_spelling.is_some());
                pairs.insert(key.as_bytes(), bytes.as_slice())?;
                match &stored.remote_spelling {
                    Some(spelling) => {
                        respell.insert(key.as_bytes(), spelling.as_bytes())?;
                    }
                    None => {
                        respell.remove(key.as_bytes())?;
                    }
                }
            }
            let entries = pairs.len()?;
            drop(pairs);
            drop(respell);
            if let Some(meta) = &commit.meta {
                let mut table = write.open_table(META)?;
                let epoch = meta.semantics_epoch.to_string();
                let device = meta.binding.source_device.to_string();
                let canary = meta.canary_at.to_string();
                let writer = format!("{}/redb-{REDB_VERSION}", env!("CARGO_PKG_VERSION"));
                for (key, value) in [
                    ("schema", meta.schema.as_str()),
                    ("semantics_epoch", epoch.as_str()),
                    ("profile", meta.binding.profile.as_str()),
                    ("source", meta.binding.source.as_str()),
                    ("source_dev", device.as_str()),
                    ("remote", meta.binding.remote.as_str()),
                    ("server", meta.binding.server.as_str()),
                    ("remote_change_signal", meta.remote_signal.as_str()),
                    ("canary_at", canary.as_str()),
                    ("writer", writer.as_str()),
                ] {
                    table.insert(key.as_bytes(), value.as_bytes())?;
                }
            }
            Ok(entries)
        })();
        let entries = match applied {
            Ok(entries) => entries,
            Err(error) => {
                // The previous commit stays: an aborted write transaction changes nothing.
                if let Err(abort) = write.abort() {
                    self.notes.push(IndexIssue::warning(
                        "pair-index-store-failed",
                        paths.label(),
                        format!("the failed write could not be abandoned cleanly: {abort}"),
                    ));
                }
                let issue = failed(error);
                self.close();
                return Err(issue);
            }
        };
        if let Err(error) = write.commit() {
            let issue = failed(error.into());
            self.close();
            return Err(issue);
        }
        self.entries = Some(entries);
        if let Some(meta) = commit.meta {
            self.meta = Some(Some(meta));
        }
        let length = self.file_len();
        let budget = entries
            .saturating_mul(2 * COMPACT_BYTES_PER_ENTRY)
            .saturating_add(COMPACT_SLACK_BYTES);
        if length > budget {
            let compacted = self.database.as_mut().map(Database::compact).transpose();
            let note = match compacted {
                Ok(_) => IndexIssue::info(
                    "pair-index-compacted",
                    paths.label(),
                    format!(
                        "{length} bytes for {entries} pairs exceeded {budget}; compacted to {}",
                        self.file_len()
                    ),
                ),
                Err(error) => IndexIssue::info(
                    "pair-index-compaction-failed",
                    paths.label(),
                    format!("compaction failed and the file stays as it is: {error}"),
                ),
            };
            self.notes.push(note);
        }
        self.close();
        Ok(())
    }

    fn stored_pairs(&mut self) -> Option<u64> {
        if let Some(entries) = self.entries {
            return Some(entries);
        }
        self.read_tables().ok()?;
        let entries = self.pairs.as_ref().and_then(|table| table.len().ok());
        self.entries = entries;
        entries
    }

    fn take_notes(&mut self) -> Vec<IndexIssue> {
        std::mem::take(&mut self.notes)
    }

    fn take_discarded(&mut self) -> bool {
        std::mem::take(&mut self.discarded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_with_every_optional_field() {
        let record = PairRecord {
            size: 1 << 40,
            local_mtime_ns: -5,
            local_ctime_ns: 1_800_000_000_123_456_789,
            device: u64::MAX,
            inode: 7,
            remote_mtime: 1_700_000_000,
            remote_ctime: Some(1_700_000_001),
            remote_crtime: None,
            md5: [9; 16],
            verified_at: 1_800_000_000,
            verified_at_nas: Some(1_800_000_002),
            racy: true,
        };
        let bytes = encode_record(&record, true);
        assert_eq!(decode_record(&bytes), Some((record, true)));
        let plain = PairRecord {
            remote_ctime: None,
            verified_at_nas: None,
            racy: false,
            ..record
        };
        assert_eq!(
            decode_record(&encode_record(&plain, false)),
            Some((plain, false))
        );
        assert_eq!(decode_record(&bytes[..103]), None);
        let mut unknown_flag = bytes;
        unknown_flag[103] = 0x80;
        assert_eq!(decode_record(&unknown_flag), None);
    }

    #[test]
    fn only_the_indexs_own_names_are_claimed() {
        for owned in [
            "photos.redb",
            "photos.redb.corrupt-1800000000",
            "photos.redb.panicked",
            MARKER,
        ] {
            assert!(is_index_owned(std::ffi::OsStr::new(owned)), "{owned}");
        }
        for foreign in [
            "notes.txt",
            "../x.redb",
            ".hidden.redb",
            "photos.redb.corrupt-",
            "photos.redb.meta",
            ".x.redb.panicked",
        ] {
            assert!(!is_index_owned(std::ffi::OsStr::new(foreign)), "{foreign}");
        }
    }

    fn scratch(label: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = std::env::temp_dir().join(format!(
            "sdsync-redb-unit-{label}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        prepare_directory(&directory).unwrap();
        directory
    }

    fn one_pair() -> IndexCommit {
        let record = PairRecord {
            size: 1,
            local_mtime_ns: 1,
            local_ctime_ns: 1,
            device: 1,
            inode: 1,
            remote_mtime: 1,
            remote_ctime: Some(1),
            remote_crtime: None,
            md5: [1; 16],
            verified_at: 1,
            verified_at_nas: Some(1),
            racy: false,
        };
        IndexCommit {
            upsert: vec![(
                "a".to_owned(),
                StoredPair {
                    record,
                    remote_spelling: None,
                },
            )],
            ..IndexCommit::default()
        }
    }

    /// D4: damage redb found and repaired on a reopen used to be noted ("its evidence was
    /// cleared") while the run went on reading the repaired file, and nothing cleared it if the
    /// run then abandoned. Now the file is set aside on the spot, the session is told, the note
    /// says what happened, and what remains is a new, empty, verified file.
    #[test]
    fn a_repair_found_on_a_reopen_sets_the_file_aside_and_serves_nothing() {
        let directory = scratch("repaired");
        let paths = IndexPaths::new(&directory, "photos").unwrap();
        let mut backend = RedbPairIndex::open(paths.clone(), 8).backend.unwrap();
        backend.commit(one_pair()).unwrap();
        assert!(backend.get("a").unwrap().is_some());
        backend.finish_reads();
        // What a reopen reports when redb repaired damage.
        backend
            .settle(None, CheckVerdict::Passed { repaired: true }, true)
            .unwrap();
        assert!(backend.take_discarded(), "the session is told");
        assert!(!backend.take_discarded(), "once");
        assert_eq!(backend.get("a").unwrap(), None, "nothing from it is read");
        let notes = backend.take_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0].code, "pair-index-corrupt");
        assert!(
            notes[0].cause.contains("while the run worked")
                && notes[0].cause.contains("set aside as")
                && !notes[0].cause.contains("was cleared"),
            "{}",
            notes[0].cause
        );
        let copies = fs::read_dir(&directory)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(SUFFIX_CORRUPT))
            .count();
        assert_eq!(copies, 1);
        fs::remove_dir_all(&directory).unwrap();
    }
}
