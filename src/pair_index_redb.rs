//! The verified-pair index on disk: one redb database per profile (perf-design §2.2, §2.7, §9).
//!
//! redb 4.4 reports a truncated, foreign or locked file as an error at open, so the file is
//! opened in this process. It still panics when a read meets a corrupted page that has not been
//! verified (perf-design measured 29 of 80 random corruptions), so every open is an
//! [`open_verified`]: `check_integrity()` verifies every page checksum (and repairs an
//! interrupted commit) before any table is read. A file that fails is set aside as
//! `<profile>.redb.corrupt-<epoch>` and rebuilt from the run's own evidence; one redb had to
//! repair is cleared in the run's commit. Every key and value is `&[u8]` and checked as UTF-8
//! here, so no stored byte can reach a decoder that unwraps.
//!
//! The file is held in two short windows: W1 reads `meta` and the pairs the comparison set names,
//! then closes; W2, at the end of the run, reopens and writes one transaction. Between them the
//! file is unlocked for however long the run takes, so a dashboard read is never queued behind a
//! sync.
//!
//! Every way the index cannot be used is a named [`IndexIssue`] and a fully live run; none fails
//! a run or makes it partial.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
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
/// Default redb cache. Mandatory: redb's 1 GiB default reached +642 MB RSS at 1M entries.
pub const DEFAULT_CACHE_MIB: u64 = 8;
pub const MAX_CACHE_MIB: u64 = 64;
/// How long the core waits for another holder of the file (a dashboard read) to let go.
pub const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOCK_RETRY: Duration = Duration::from_millis(250);
/// Quarantined copies older than this are swept.
const QUARANTINE_AGE: Duration = Duration::from_secs(7 * 86_400);
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
    name.strip_suffix(SUFFIX_DATABASE)
        .is_some_and(crate::evidence_dir::name_is_safe)
}

/// Make `directory` the index's own: created 0700 when missing, private, and claimed by marker.
pub fn prepare_directory(directory: &Path) -> Result<(), IndexIssue> {
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
    sweep_quarantine(directory);
    Ok(())
}

/// Remove quarantined copies past [`QUARANTINE_AGE`]. Best effort: a copy that stays is harmless.
fn sweep_quarantine(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let is_copy = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.contains(SUFFIX_CORRUPT));
        let old = entry
            .metadata()
            .ok()
            .filter(fs::Metadata::is_file)
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > QUARANTINE_AGE);
        if is_copy && old {
            let _ = fs::remove_file(entry.path());
        }
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
fn quarantine(paths: &IndexPaths) -> Result<PathBuf, String> {
    if let Ok(entries) = fs::read_dir(&paths.directory) {
        let prefix = format!("{}{SUFFIX_CORRUPT}", paths.profile);
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let target = paths
        .directory
        .join(format!("{}{SUFFIX_CORRUPT}{}", paths.profile, unix_now()));
    fs::rename(&paths.database, &target)
        .map_err(|error| format!("could not be set aside as {}: {error}", target.display()))?;
    Ok(target)
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
/// The only way this module obtains a readable handle: redb 4.4 can panic reading a page that
/// is corrupted and unverified, so no table is opened on a file `check_integrity()` has not
/// passed in this same open.
pub fn open_verified(paths: &IndexPaths, cache_mib: u64) -> (Option<Database>, CheckVerdict) {
    let mut database = match open_database(paths, cache_mib) {
        Ok(database) => database,
        Err(Opening::Issue(issue)) => return (None, CheckVerdict::Unsafe(issue)),
        Err(Opening::Redb(error)) => return (None, verdict_for(error)),
    };
    match database.check_integrity() {
        Ok(clean) => (Some(database), CheckVerdict::Passed { repaired: !clean }),
        Err(error) => (None, verdict_for(error.into())),
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

/// Open the database, waiting up to [`LOCK_WAIT`] for another holder.
fn open_database(paths: &IndexPaths, cache_mib: u64) -> Result<Database, Opening> {
    let started = Instant::now();
    loop {
        let file = open_file_safely(paths).map_err(Opening::Issue)?;
        match builder(cache_mib).create_file(file) {
            Ok(database) => return Ok(database),
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
    /// A reopen found and repaired damage: the commit clears every pair.
    repaired_since_open: bool,
}

/// What opening the index for a run produced: a backend, or none, and the issues met on the way.
pub struct Opened {
    pub backend: Option<RedbPairIndex>,
    /// The file's evidence must not be served and is cleared in the run's commit.
    pub clear: bool,
    pub issues: Vec<IndexIssue>,
}

impl RedbPairIndex {
    /// Open the index, verified. Corrupt and foreign-format files become an empty index (the
    /// file set aside first); a file redb had to repair is cleared by the run's commit; locked,
    /// read-only and unsafe files give no backend at all. Every case but a clean pass is named.
    pub fn open(paths: IndexPaths, cache_mib: u64) -> Opened {
        let mut issues = Vec::new();
        let label = paths.label();
        let refused = |issues: Vec<IndexIssue>| Opened {
            backend: None,
            clear: false,
            issues,
        };
        let (mut database, verdict) = open_verified(&paths, cache_mib);
        let mut clear = false;
        match verdict {
            CheckVerdict::Passed { repaired } => {
                if repaired {
                    // Repaired damage may have been anything: none of the evidence is trusted.
                    clear = true;
                    issues.push(IndexIssue::warning(
                        "pair-index-corrupt",
                        label.clone(),
                        "redb found and repaired damage in the file; its evidence is not served \
                         and is replaced by this run's",
                    ));
                }
            }
            CheckVerdict::Corrupt(cause) | CheckVerdict::Unsupported(cause) => {
                let code = if cause.starts_with("file format") {
                    "pair-index-format-unsupported"
                } else {
                    "pair-index-corrupt"
                };
                match quarantine(&paths) {
                    Ok(target) => issues.push(IndexIssue::warning(
                        code,
                        label.clone(),
                        format!(
                            "{cause}; set aside as {} and rebuilt from this run's evidence",
                            target.display()
                        ),
                    )),
                    Err(why) => {
                        issues.push(IndexIssue::warning(code, label, format!("{cause}; {why}")));
                        return refused(issues);
                    }
                }
                // A new, empty file in its place.
                let (fresh, verdict) = open_verified(&paths, cache_mib);
                match verdict {
                    CheckVerdict::Passed { .. } => database = fresh,
                    other => {
                        issues.push(IndexIssue::warning(
                            "pair-index-store-failed",
                            paths.label(),
                            format!("a new index could not be created: {other:?}"),
                        ));
                        return refused(issues);
                    }
                }
            }
            CheckVerdict::Locked => {
                issues.push(issue_for(&paths, redb::Error::DatabaseAlreadyOpen, false));
                return refused(issues);
            }
            CheckVerdict::ReadOnly(cause) => {
                issues.push(IndexIssue::warning("pair-index-readonly", label, cause));
                return refused(issues);
            }
            CheckVerdict::Unsafe(issue) => {
                issues.push(issue);
                return refused(issues);
            }
        }
        let backend = Self {
            paths,
            cache_mib,
            database,
            pairs: None,
            respell: None,
            meta: None,
            notes: Vec::new(),
            repaired_since_open: false,
        };
        Opened {
            backend: Some(backend),
            clear,
            issues,
        }
    }

    /// Reopen for another window, verified as every open is.
    fn ensure_open(&mut self) -> Result<(), IndexIssue> {
        if self.database.is_some() {
            return Ok(());
        }
        let (database, verdict) = open_verified(&self.paths, self.cache_mib);
        match verdict {
            CheckVerdict::Passed { repaired } => {
                if repaired {
                    self.repaired_since_open = true;
                    self.notes.push(IndexIssue::warning(
                        "pair-index-corrupt",
                        self.paths.label(),
                        "redb found and repaired damage in the file while the run worked; its \
                         evidence was cleared",
                    ));
                }
                self.database = database;
                Ok(())
            }
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
            CheckVerdict::Corrupt(cause) | CheckVerdict::Unsupported(cause) => Err(
                IndexIssue::warning("pair-index-corrupt", self.paths.label(), cause),
            ),
        }
    }

    /// Open the read transaction's tables, once per read window.
    fn read_tables(&mut self) -> Result<(), IndexIssue> {
        if self.pairs.is_some() {
            return Ok(());
        }
        self.ensure_open()?;
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

    fn read_meta(&mut self) -> Result<Option<IndexMeta>, IndexIssue> {
        self.ensure_open()?;
        let database = self.database.as_ref().expect("opened above");
        let read = database
            .begin_read()
            .map_err(|error| issue_for(&self.paths, error.into(), false))?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(issue_for(&self.paths, error.into(), false)),
        };
        let get = |key: &str| -> Result<Option<String>, IndexIssue> {
            let Some(value) = table
                .get(key.as_bytes())
                .map_err(|error| issue_for(&self.paths, error.into(), false))?
            else {
                return Ok(None);
            };
            String::from_utf8(value.value().to_vec())
                .map(Some)
                .map_err(|_| {
                    IndexIssue::warning(
                        "pair-index-corrupt",
                        self.paths.label(),
                        format!("meta {key:?} is not UTF-8"),
                    )
                })
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

    /// Close the file: the read window is over.
    fn close(&mut self) {
        self.pairs = None;
        self.respell = None;
        self.database = None;
    }

    fn file_len(&self) -> u64 {
        fs::metadata(&self.paths.database).map_or(0, |metadata| metadata.len())
    }
}

impl PairIndex for RedbPairIndex {
    fn meta(&mut self) -> Result<Option<IndexMeta>, IndexIssue> {
        if let Some(meta) = &self.meta {
            return Ok(meta.clone());
        }
        let meta = self.read_meta()?;
        self.meta = Some(meta.clone());
        Ok(meta)
    }

    fn get(&mut self, local_relative: &str) -> Result<Option<StoredPair>, IndexIssue> {
        self.read_tables()?;
        let Some(pairs) = &self.pairs else {
            return Ok(None);
        };
        let Some(bytes) = pairs
            .get(local_relative.as_bytes())
            .map_err(|error| issue_for(&self.paths, error.into(), false))?
        else {
            return Ok(None);
        };
        let Some((record, respelled)) = decode_record(bytes.value()) else {
            return Err(IndexIssue::warning(
                "pair-index-corrupt",
                self.paths.label(),
                format!("the record for {local_relative:?} is not a valid pair record"),
            ));
        };
        let remote_spelling = if respelled {
            let spelling = match &self.respell {
                Some(table) => table
                    .get(local_relative.as_bytes())
                    .map_err(|error| issue_for(&self.paths, error.into(), false))?
                    .and_then(|value| String::from_utf8(value.value().to_vec()).ok()),
                None => None,
            };
            if spelling.is_none() {
                return Err(IndexIssue::warning(
                    "pair-index-corrupt",
                    self.paths.label(),
                    format!("the record for {local_relative:?} names a spelling that is missing"),
                ));
            }
            spelling
        } else {
            None
        };
        Ok(Some(StoredPair {
            record,
            remote_spelling,
        }))
    }

    fn for_each_key(&mut self, visit: &mut dyn FnMut(&str)) -> Result<(), IndexIssue> {
        self.read_tables()?;
        let Some(pairs) = &self.pairs else {
            return Ok(());
        };
        let range = pairs
            .iter()
            .map_err(|error| issue_for(&self.paths, error.into(), false))?;
        for entry in range {
            // Stop at the first error rather than skip what could not be read.
            let (key, _) = entry.map_err(|error| issue_for(&self.paths, error.into(), false))?;
            let Ok(key) = std::str::from_utf8(key.value()) else {
                return Err(IndexIssue::warning(
                    "pair-index-corrupt",
                    self.paths.label(),
                    "a stored path is not UTF-8",
                ));
            };
            visit(key);
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
        let clear_repaired = std::mem::take(&mut self.repaired_since_open);
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
            if commit.clear || clear_repaired {
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
                let _ = write.abort();
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
        self.read_tables().ok()?;
        self.pairs.as_ref().and_then(|table| table.len().ok())
    }

    fn take_notes(&mut self) -> Vec<IndexIssue> {
        std::mem::take(&mut self.notes)
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
        for owned in ["photos.redb", "photos.redb.corrupt-1800000000", MARKER] {
            assert!(is_index_owned(std::ffi::OsStr::new(owned)), "{owned}");
        }
        for foreign in [
            "notes.txt",
            "../x.redb",
            ".hidden.redb",
            "photos.redb.corrupt-",
            "photos.redb.meta",
        ] {
            assert!(!is_index_owned(std::ffi::OsStr::new(foreign)), "{foreign}");
        }
    }
}
