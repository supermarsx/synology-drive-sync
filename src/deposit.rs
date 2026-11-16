//! Crash-atomic deposit: a file appears under its final NAS name only once it is complete and
//! verified, and a crash at any point leaves the old version or the new one, never a partial file.
//! Only a replacement interrupted between its two renames leaves the final name empty, with the old
//! version whole under its reserved name until the next run puts it back.
//!
//! # The protocol
//!
//! An upload goes to a temporary name in the destination's own folder
//! (`~sdsync-part-<owner>-<run>-<seq>`), is verified there (length, the modification time sent,
//! and File Station's MD5 against a fingerprint of the bytes that went onto the wire), and is then
//! published:
//!
//! - **a new file**: one `Rename` of the temporary copy to the final name. DSM's Rename refuses a
//!   name that is taken (414), so a file someone else created meanwhile is never overwritten: the
//!   temporary copy is removed and the path deferred as `remote-changed`;
//! - **an overwrite**: the destination is checked once more against what the plan saw, renamed
//!   aside to `~sdsync-prev-<owner>-<run>-<seq>`, the temporary copy renamed to the final name, and
//!   the previous version deleted. Between the two renames the final name is absent for one round
//!   trip; the bytes never are: the old version or the new one exists under some name at every
//!   instant, and recovery restores the old one if the process dies in between.
//!
//! # The journal
//!
//! Before anything is written, every deposit this run may make is declared in a local journal
//! (one fsync for the whole plan; uploads the run discovers later are declared one at a time). The
//! journal is per owner -- a profile, its NAS destination, the server and the account -- and is
//! held under an exclusive lock for the whole run, so two runs of one profile cannot interleave.
//! A run that ends normally removes it, or keeps exactly the entries it could not settle; a run
//! that is killed leaves it whole.
//!
//! # Recovery
//!
//! Every run first settles what an earlier run left: for each journalled deposit it removes an
//! unpublished temporary copy, rolls a moved-aside previous version back when the final name is
//! empty, and deletes it when the final name holds the new version (size, time and MD5 as
//! journalled). A final name that holds anything else is someone else's write: both are kept and
//! named. Only names this owner's journal lists are ever touched; a reserved name the journal does
//! not list is reported and left alone.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::api::{RemotePathState, UploadObserver, UploadTransferEvent};
use crate::cancel::CancellationToken;
use crate::integrity::ContentMd5;
use crate::local::LocalEntry;
use crate::path::{RemoteRoot, parent_and_name};
use crate::plan::RemoteSnapshot;
use crate::{Error, Result};

/// The journal's format version. A journal of another version is never acted on.
pub const JOURNAL_VERSION: u32 = 1;

/// Name prefix of an upload's temporary copy.
pub const TEMP_PREFIX: &str = "~sdsync-part-";
/// Name prefix of the previous version an overwrite moves aside.
pub const BACKUP_PREFIX: &str = "~sdsync-prev-";

/// How files are deposited on the NAS.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DepositMode {
    /// Temporary name, verify, publish by rename; journalled and recovered. The default.
    #[default]
    Atomic,
    /// Upload straight to the final name, as before atomic deposit existed. An explicit opt-out:
    /// a run killed mid-upload can leave a partial or torn file under the final name. Every run
    /// in this mode is named `deposit-not-atomic`.
    Direct,
}

impl DepositMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Atomic => "atomic",
            Self::Direct => "direct",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "atomic" => Ok(Self::Atomic),
            "direct" => Ok(Self::Direct),
            other => Err(Error::Configuration(format!(
                "deposit must be \"atomic\" or \"direct\", not {other:?}"
            ))),
        }
    }
}

/// Who owns a set of deposits: a profile, its NAS destination, the server and the account. Twelve
/// hex digits of a SHA-256 over them, carried in every reserved name this owner creates, so one
/// profile's recovery can never take another's file for its own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositOwner(String);

impl DepositOwner {
    pub fn derive(profile: &str, remote_root: &str, server: &str, account: &str) -> Self {
        let mut hasher = Sha256::new();
        for part in ["sdsync-deposit-v1", profile, remote_root, server, account] {
            hasher.update(part.as_bytes());
            hasher.update([0_u8]);
        }
        let digest = hasher.finalize();
        Self(
            digest[..6]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A run identifier: twelve hex digits from the clock and the process id. Runs of one owner are
/// serialised by the journal lock, so this only has to differ from the previous run's.
pub fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let mixed = (nanos as u64) ^ (u64::from(std::process::id()) << 40);
    format!("{:012x}", mixed & 0xffff_ffff_ffff)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// What a journalled deposit writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntentKind {
    /// An upload through a temporary name.
    Upload,
    /// A server-side copy, written by DSM straight to the final name (non-overwriting).
    Copy,
}

/// One declared deposit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    /// The run that declared it.
    pub run: String,
    pub seq: u64,
    /// The absolute NAS path it publishes.
    #[serde(rename = "final")]
    pub final_path: String,
    pub kind: IntentKind,
    pub size: u64,
    /// The MD5 of the content, lower-case hex, when the run knew it.
    pub md5: Option<String>,
    pub mtime_ms: i64,
}

impl Intent {
    fn sibling(&self, owner: &DepositOwner, prefix: &str) -> Result<String> {
        let (parent, _) = parent_and_name(&self.final_path)?;
        Ok(format!(
            "{parent}/{prefix}{}-{}-{}",
            owner.as_str(),
            self.run,
            self.seq
        ))
    }

    /// Where the upload's temporary copy lives.
    pub fn temp_path(&self, owner: &DepositOwner) -> Result<String> {
        self.sibling(owner, TEMP_PREFIX)
    }

    /// Where an overwrite moves the previous version.
    pub fn backup_path(&self, owner: &DepositOwner) -> Result<String> {
        self.sibling(owner, BACKUP_PREFIX)
    }

    fn content_md5(&self) -> Option<ContentMd5> {
        self.md5
            .as_deref()
            .and_then(|hex| ContentMd5::parse_hex(hex).ok())
    }

    fn key(&self) -> (String, u64) {
        (self.run.clone(), self.seq)
    }
}

/// What a run is about to deposit, before it has a sequence number.
#[derive(Clone, Debug)]
pub struct NewIntent {
    pub final_path: String,
    pub kind: IntentKind,
    pub size: u64,
    pub md5: Option<ContentMd5>,
    pub mtime_ms: i64,
}

impl NewIntent {
    pub fn upload(local: &LocalEntry, final_path: &str) -> Self {
        Self {
            final_path: final_path.to_owned(),
            kind: IntentKind::Upload,
            size: local.size,
            md5: local.content_md5,
            mtime_ms: local.mtime_ms,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    v: u32,
    owner: String,
    run: String,
    started: i64,
    remote_root: String,
}

/// How serious a deposit issue is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Info,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
        }
    }
}

/// One thing the deposit did or could not do, by name. Every recovery action and every failure
/// to clean up is one of these, with the NAS path and the cause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositIssue {
    /// One of [`ISSUE_CODES`].
    pub code: &'static str,
    pub severity: Severity,
    pub path: String,
    pub cause: String,
}

/// Every issue code the deposit can name: the closed vocabulary the result record publishes.
pub const ISSUE_CODES: [&str; 13] = [
    "deposit-not-atomic",
    "deposit-not-atomic-unavailable",
    "deposit-temp-removed",
    "deposit-rolled-back",
    "deposit-completed",
    "deposit-backup-kept",
    "deposit-recovery-failed",
    "deposit-cleanup-failed",
    "deposit-orphan-unjournaled",
    "deposit-copy-partial-removed",
    "deposit-copy-kept",
    "deposit-journal-torn",
    "deposit-mtime-not-kept",
];

impl DepositIssue {
    fn new(code: &'static str, severity: Severity, path: &str, cause: impl Into<String>) -> Self {
        debug_assert!(ISSUE_CODES.contains(&code), "{code}");
        Self {
            code,
            severity,
            path: path.to_owned(),
            cause: cause.into(),
        }
    }

    pub fn warning(code: &'static str, path: &str, cause: impl Into<String>) -> Self {
        Self::new(code, Severity::Warning, path, cause)
    }

    pub fn info(code: &'static str, path: &str, cause: impl Into<String>) -> Self {
        Self::new(code, Severity::Info, path, cause)
    }
}

/// What the deposit did in one run, for the result record and the log.
#[derive(Clone, Debug, Default)]
pub struct DepositReport {
    pub mode: DepositMode,
    /// The journal file, when one was used.
    pub journal: Option<PathBuf>,
    /// Files published under a name that was empty.
    pub published: usize,
    /// Files published over a previous version.
    pub replaced: usize,
    /// Earlier runs' deposits settled by this run's recovery.
    pub recovered: usize,
    /// Journalled deposits still unsettled when the run ended (carried to the next run).
    pub unsettled: usize,
    pub issues: Vec<DepositIssue>,
    /// NAS files recovery kept but could not prove whole (an interrupted server copy): this run
    /// uploads over them whatever its comparison says (`plan::force_uploads`).
    pub suspect: BTreeSet<String>,
}

impl DepositReport {
    pub fn off(mode: DepositMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Whether any issue is a warning.
    pub fn has_warnings(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == Severity::Warning)
    }
}

// ------------------------------------------------------------------------------------------------
// Fault injection
// ------------------------------------------------------------------------------------------------

/// `SDSYNC_FAULT_ABORT=deposit:<step>[:<n>]` aborts the process -- no unwinding, no destructors,
/// no journal close, the way `kill -9` or a power loss ends it -- the `n`th time (default the
/// first) the deposit reaches `<step>`. Compiled only with the `fault-inject` feature, never into a
/// release. Steps: `journal-declared`, `temp-streaming`, `temp-verified`, `backup-renamed`,
/// `published`, `backup-deleted`, `before-journal-close`, `recovery-rolled-back`.
#[cfg(feature = "fault-inject")]
pub(crate) fn fault(step: &str) {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static HITS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    let Some(rule) = std::env::var_os("SDSYNC_FAULT_ABORT") else {
        return;
    };
    let rule = rule.to_string_lossy().into_owned();
    let Some(rest) = rule.strip_prefix("deposit:") else {
        return;
    };
    let (wanted, nth) = match rest.rsplit_once(':') {
        Some((wanted, nth)) if nth.parse::<usize>().is_ok() => {
            (wanted, nth.parse::<usize>().unwrap_or(1))
        }
        _ => (rest, 1),
    };
    if wanted != step {
        return;
    }
    let mut hits = HITS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let count = hits.entry(step.to_owned()).or_insert(0);
    *count += 1;
    if *count == nth {
        eprintln!("fault-inject: aborting at deposit step {step} (hit {nth})");
        std::process::abort();
    }
}

#[cfg(not(feature = "fault-inject"))]
#[inline(always)]
pub(crate) fn fault(_step: &str) {}

// ------------------------------------------------------------------------------------------------
// The journal
// ------------------------------------------------------------------------------------------------

/// The per-user directory a journal lives in when none is configured:
/// `$XDG_STATE_HOME/synology-drive-sync/deposit` (or `~/.local/state/...`) on Linux and other
/// Unix systems, `~/Library/Application Support/synology-drive-sync/deposit` on macOS and
/// `%LOCALAPPDATA%\synology-drive-sync\deposit` on Windows.
pub fn default_journal_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|base| base.join("synology-drive-sync").join("deposit"));
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME").map(PathBuf::from).map(|base| {
            base.join("Library")
                .join("Application Support")
                .join("synology-drive-sync")
                .join("deposit")
        });
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(base) = std::env::var_os("XDG_STATE_HOME").filter(|base| !base.is_empty()) {
            return Some(
                PathBuf::from(base)
                    .join("synology-drive-sync")
                    .join("deposit"),
            );
        }
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|base| base.join(".local/state/synology-drive-sync/deposit"));
    }
    #[allow(unreachable_code)]
    None
}

fn journal_error(code: &'static str, path: &Path, detail: impl Into<String>) -> Error {
    Error::DepositJournal {
        code,
        path: path.to_owned(),
        detail: detail.into(),
    }
}

/// An open, locked journal for one owner and one run.
#[derive(Debug)]
pub struct DepositJournal {
    owner: DepositOwner,
    run: String,
    started: i64,
    remote_root: String,
    dir: PathBuf,
    path: PathBuf,
    /// Held for the whole run; released when this is dropped.
    _lock: File,
    state: Mutex<JournalState>,
}

#[derive(Debug, Default)]
struct JournalState {
    file: Option<File>,
    next_seq: u64,
    unresolved: BTreeMap<(String, u64), Intent>,
    closed: bool,
}

/// What [`DepositJournal::open`] found from earlier runs.
#[derive(Debug, Default)]
pub struct PriorJournal {
    /// Every deposit an earlier run declared and did not settle.
    pub intents: Vec<Intent>,
    /// When the earliest of those runs started (Unix seconds).
    pub started: Option<i64>,
    pub issues: Vec<DepositIssue>,
}

impl DepositJournal {
    /// Claim the journal directory and this owner's lock, and read what earlier runs left.
    ///
    /// Fails, and nothing may be uploaded, when the directory cannot be made private, the lock
    /// is held by another run of this profile, or an earlier journal cannot be read: acting
    /// without it could leave a deposit nobody recovers, or recover one that is still in flight.
    pub fn open(
        dir: &Path,
        owner: &DepositOwner,
        remote_root: &str,
    ) -> Result<(Self, PriorJournal)> {
        prepare_directory(dir)?;
        let lock_path = dir.join(format!("{}.lock", owner.as_str()));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|error| {
                journal_error(
                    "deposit-journal-unavailable",
                    &lock_path,
                    format!("cannot open the lock file: {error}"),
                )
            })?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(journal_error(
                    "deposit-journal-locked",
                    &lock_path,
                    "another run of this profile holds the deposit journal; two runs of one \
                     profile must not write the same destination at once",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(journal_error(
                    "deposit-journal-unavailable",
                    &lock_path,
                    format!("cannot lock the journal: {error}"),
                ));
            }
        }
        let path = dir.join(format!("{}.journal", owner.as_str()));
        let prior = read_journal(&path, owner)?;
        let journal = Self {
            owner: owner.clone(),
            run: new_run_id(),
            started: now_seconds(),
            remote_root: remote_root.to_owned(),
            dir: dir.to_owned(),
            path,
            _lock: lock,
            state: Mutex::new(JournalState {
                next_seq: 1,
                ..JournalState::default()
            }),
        };
        Ok((journal, prior))
    }

    pub fn owner(&self) -> &DepositOwner {
        &self.owner
    }

    pub fn run(&self) -> &str {
        &self.run
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start this run's journal, carrying forward what recovery could not settle. Replaces the
    /// previous journal atomically: a crash here leaves the previous one or this one.
    pub fn begin(&self, carried: Vec<Intent>) -> Result<()> {
        let mut state = self.lock_state();
        let header = Header {
            v: JOURNAL_VERSION,
            owner: self.owner.as_str().to_owned(),
            run: self.run.clone(),
            started: self.started,
            remote_root: self.remote_root.clone(),
        };
        let mut text = serialize_line(&header, &self.path)?;
        for intent in &carried {
            text.push_str(&serialize_line(intent, &self.path)?);
        }
        let temporary = self
            .dir
            .join(format!("{}.journal.new", self.owner.as_str()));
        write_synced(&temporary, text.as_bytes())?;
        fs::rename(&temporary, &self.path).map_err(|error| {
            journal_error(
                "deposit-journal-unavailable",
                &self.path,
                format!("cannot install the journal: {error}"),
            )
        })?;
        sync_directory(&self.dir);
        let file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|error| {
                journal_error(
                    "deposit-journal-unavailable",
                    &self.path,
                    format!("cannot reopen the journal: {error}"),
                )
            })?;
        state.file = Some(file);
        for intent in carried {
            state.unresolved.insert(intent.key(), intent);
        }
        Ok(())
    }

    /// Declare deposits, durably, before any of them is written: one write and one `fsync` for
    /// the whole batch.
    pub fn declare(&self, entries: &[NewIntent]) -> Result<Vec<Intent>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = self.lock_state();
        let mut text = String::new();
        let mut intents = Vec::with_capacity(entries.len());
        for entry in entries {
            let intent = Intent {
                run: self.run.clone(),
                seq: state.next_seq,
                final_path: entry.final_path.clone(),
                kind: entry.kind,
                size: entry.size,
                md5: entry.md5.map(|md5| md5.to_string()),
                mtime_ms: entry.mtime_ms,
            };
            state.next_seq += 1;
            text.push_str(&serialize_line(&intent, &self.path)?);
            intents.push(intent);
        }
        let Some(file) = state.file.as_mut() else {
            return Err(journal_error(
                "deposit-journal-unavailable",
                &self.path,
                "the journal was not started",
            ));
        };
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_data())
            .map_err(|error| {
                journal_error(
                    "deposit-journal-unavailable",
                    &self.path,
                    format!("cannot record what this run is about to write: {error}"),
                )
            })?;
        for intent in &intents {
            state.unresolved.insert(intent.key(), intent.clone());
        }
        drop(state);
        fault("journal-declared");
        Ok(intents)
    }

    /// The NAS is known to hold nothing of this deposit but, at most, its published file.
    pub fn resolve(&self, intent: &Intent) {
        self.lock_state().unresolved.remove(&intent.key());
    }

    /// How many declared deposits are not settled.
    pub fn unsettled(&self) -> usize {
        self.lock_state().unresolved.len()
    }

    /// End the run: remove the journal when every deposit is settled, or rewrite it with exactly
    /// the unsettled ones, for the next run's recovery. Returns how many were carried.
    pub fn close(&self) -> Result<usize> {
        fault("before-journal-close");
        let mut state = self.lock_state();
        if state.closed {
            return Ok(state.unresolved.len());
        }
        state.closed = true;
        state.file = None;
        let carried: Vec<Intent> = state.unresolved.values().cloned().collect();
        if carried.is_empty() {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(journal_error(
                        "deposit-journal-unavailable",
                        &self.path,
                        format!("cannot remove the settled journal: {error}"),
                    ));
                }
            }
            sync_directory(&self.dir);
            return Ok(0);
        }
        let header = Header {
            v: JOURNAL_VERSION,
            owner: self.owner.as_str().to_owned(),
            run: self.run.clone(),
            started: self.started,
            remote_root: self.remote_root.clone(),
        };
        let mut text = serialize_line(&header, &self.path)?;
        for intent in &carried {
            text.push_str(&serialize_line(intent, &self.path)?);
        }
        let temporary = self
            .dir
            .join(format!("{}.journal.new", self.owner.as_str()));
        write_synced(&temporary, text.as_bytes())?;
        fs::rename(&temporary, &self.path).map_err(|error| {
            journal_error(
                "deposit-journal-unavailable",
                &self.path,
                format!("cannot install the journal: {error}"),
            )
        })?;
        sync_directory(&self.dir);
        Ok(carried.len())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, JournalState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn serialize_line<T: Serialize>(value: &T, path: &Path) -> Result<String> {
    let mut line = serde_json::to_string(value).map_err(|error| {
        journal_error(
            "deposit-journal-unavailable",
            path,
            format!("cannot encode a journal record: {error}"),
        )
    })?;
    line.push('\n');
    Ok(line)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .map_err(|error| {
            journal_error(
                "deposit-journal-unavailable",
                path,
                format!("cannot write the journal: {error}"),
            )
        })
}

/// Flush a directory entry change (a rename, a removal) to disk where the platform allows it.
fn sync_directory(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Create the journal directory private to this user, and refuse one others may write.
fn prepare_directory(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    if let Err(error) = builder.create(dir) {
        return Err(journal_error(
            "deposit-journal-unavailable",
            dir,
            format!("cannot create the journal directory: {error}"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = fs::symlink_metadata(dir).map_err(|error| {
            journal_error(
                "deposit-journal-unavailable",
                dir,
                format!("cannot examine the journal directory: {error}"),
            )
        })?;
        if !metadata.is_dir() {
            return Err(journal_error(
                "deposit-journal-unsafe",
                dir,
                "the journal directory is not a directory (a symbolic link is refused)",
            ));
        }
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(journal_error(
                "deposit-journal-unsafe",
                dir,
                format!(
                    "the journal directory grants write access to group or other (mode {:o}); \
                     it decides which NAS files this tool may remove, so it must be private",
                    mode & 0o7777
                ),
            ));
        }
    }
    Ok(())
}

fn read_journal(path: &Path, owner: &DepositOwner) -> Result<PriorJournal> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PriorJournal::default());
        }
        Err(error) => {
            return Err(journal_error(
                "deposit-journal-unavailable",
                path,
                format!("cannot read the earlier run's journal: {error}"),
            ));
        }
    };
    let corrupt = |detail: String| {
        journal_error(
            "deposit-journal-corrupt",
            path,
            format!(
                "{detail}; nothing is uploaded until it is resolved: look for ~sdsync-part- and \
                 ~sdsync-prev- files in the destination, then move the journal aside"
            ),
        )
    };
    let mut lines = Vec::new();
    for line in BufReader::new(file).split(b'\n') {
        let line = line.map_err(|error| corrupt(format!("cannot read it: {error}")))?;
        lines.push(line);
    }
    let mut prior = PriorJournal::default();
    let Some(first) = lines.first() else {
        return Ok(prior);
    };
    let header: Header = serde_json::from_slice(first)
        .map_err(|error| corrupt(format!("its header is unreadable: {error}")))?;
    if header.v != JOURNAL_VERSION {
        return Err(corrupt(format!(
            "it is format version {}, this build reads {JOURNAL_VERSION}",
            header.v
        )));
    }
    if header.owner != owner.as_str() {
        return Err(corrupt(format!(
            "it belongs to owner {}, not {}",
            header.owner,
            owner.as_str()
        )));
    }
    prior.started = Some(header.started);
    let last = lines.len() - 1;
    let mut seen = BTreeSet::new();
    for (index, line) in lines.iter().enumerate().skip(1) {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Intent>(line) {
            Ok(intent) => {
                if seen.insert(intent.key()) {
                    prior.intents.push(intent);
                }
            }
            // The last record of a journal whose run lost power mid-append: the deposit it
            // describes was never started, because nothing is written before its declaration is
            // on disk.
            Err(error) if index == last => prior.issues.push(DepositIssue::info(
                "deposit-journal-torn",
                &path.display().to_string(),
                format!("the last record was incomplete and is ignored ({error})"),
            )),
            Err(error) => {
                return Err(corrupt(format!(
                    "record {} is unreadable: {error}",
                    index + 1
                )));
            }
        }
    }
    Ok(prior)
}

// ------------------------------------------------------------------------------------------------
// NAS operations the deposit needs
// ------------------------------------------------------------------------------------------------

/// The File Station operations the deposit and its recovery use. Implemented for every engine
/// client (`sync`), so the protocol runs against the real API and the engine's test doubles alike.
pub(crate) trait DepositOps {
    /// A verified upload to `remote_path` (length, time and MD5 checked after it lands).
    fn deposit_upload(
        &self,
        local: &LocalEntry,
        remote_path: &str,
        observer: Option<UploadObserver>,
        cancellation: &CancellationToken,
    ) -> Result<()>;
    fn deposit_state(&self, remote_path: &str) -> Result<RemotePathState>;
    /// `Rename` within one folder; with `verify`, one `getinfo` of the new path afterwards.
    fn deposit_rename(
        &self,
        root: &RemoteRoot,
        remote_path: &str,
        new_name: &str,
        verify: bool,
    ) -> Result<Option<RemotePathState>>;
    /// A non-recursive delete; an absent entry is success.
    fn deposit_delete(&self, root: &RemoteRoot, remote_path: &str) -> Result<()>;
    /// The names directly in `folder`, or `None` when they cannot be listed completely.
    fn deposit_list(
        &self,
        folder: &str,
        cancellation: &CancellationToken,
    ) -> Option<BTreeSet<String>>;
    /// Whether the file at `remote_path` has `size` bytes and File Station's MD5 equals `md5`.
    fn deposit_md5_matches(
        &self,
        remote_path: &str,
        size: u64,
        md5: ContentMd5,
        cancellation: &CancellationToken,
    ) -> Result<bool>;
}

/// The outcome of one atomic deposit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Deposited {
    /// Published under a name that was empty.
    New,
    /// Published over the previous version.
    Replaced,
    /// The destination is not what the plan saw (someone else wrote it, or it became a folder):
    /// nothing was published, the temporary copy is gone, and the path is deferred.
    DestinationChanged(String),
}

/// What the run carries while it deposits: the journal and the issues for its report.
#[derive(Debug)]
pub struct DepositContext {
    pub journal: DepositJournal,
    issues: Mutex<Vec<DepositIssue>>,
    published: AtomicUsize,
    replaced: AtomicUsize,
    /// Destinations deposited directly, each named (see `sync::comma_ambiguous_deposits`).
    direct: Mutex<BTreeSet<String>>,
}

impl DepositContext {
    pub fn new(journal: DepositJournal) -> Self {
        Self {
            journal,
            issues: Mutex::new(Vec::new()),
            published: AtomicUsize::new(0),
            replaced: AtomicUsize::new(0),
            direct: Mutex::new(BTreeSet::new()),
        }
    }

    /// Deposit these destinations directly: an atomic deposit of them is not safe. Each is named
    /// `deposit-not-atomic` when it is uploaded.
    pub fn deposit_directly(&self, destinations: BTreeSet<String>) {
        self.direct
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(destinations);
    }

    pub(crate) fn is_direct(&self, destination: &str) -> bool {
        self.direct
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(destination)
    }

    pub(crate) fn issue(&self, issue: DepositIssue) {
        self.issues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(issue);
    }

    /// Close the journal and report what the run's deposits did.
    pub fn finish(&self, report: &mut DepositReport) -> Result<()> {
        report.journal = Some(self.journal.path().to_owned());
        report.published += self.published.load(Ordering::Acquire);
        report.replaced += self.replaced.load(Ordering::Acquire);
        report.issues.extend(
            self.issues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain(..),
        );
        report.unsettled = self.journal.close()?;
        Ok(())
    }
}

/// The verdict for a destination the plan expected to find as `expected` (or absent).
fn destination_matches(state: RemotePathState, expected: Option<&RemoteSnapshot>) -> bool {
    match (state, expected) {
        (RemotePathState::Absent, _) => true,
        (RemotePathState::Directory, _) | (RemotePathState::File { .. }, None) => false,
        (
            RemotePathState::File {
                size,
                mtime_seconds,
            },
            Some(expected),
        ) => {
            expected.kind == crate::local::EntryKind::File
                && size == expected.size
                && mtime_seconds.is_none_or(|mtime| mtime == expected.mtime_seconds)
        }
    }
}

/// Upload `local` to `final_path` through a temporary name, verify it there, and publish it.
///
/// `intent` must already be declared in `context`'s journal. On every path out of here the
/// journal entry is resolved when -- and only when -- no temporary copy or previous version can be
/// left on the NAS; otherwise it stays for the next run's recovery and an issue names the path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn deposit_file<O: DepositOps>(
    ops: &O,
    root: &RemoteRoot,
    context: &DepositContext,
    intent: &Intent,
    local: &LocalEntry,
    expected: Option<&RemoteSnapshot>,
    observer: Option<UploadObserver>,
    cancellation: &CancellationToken,
) -> Result<Deposited> {
    let owner = context.journal.owner();
    let final_path = intent.final_path.as_str();
    let (_, final_name) = parent_and_name(final_path)?;
    let temp = intent.temp_path(owner)?;
    let (_, temp_name) = parent_and_name(&temp)?;
    let backup = intent.backup_path(owner)?;
    let (_, backup_name) = parent_and_name(&backup)?;

    // The temporary copy. A fault hook on the first transferred byte stands for a process killed
    // mid-body; the user's observer still sees every event.
    let streaming_observer: Option<UploadObserver> = {
        let inner = observer;
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        Some(std::sync::Arc::new(move |event| {
            if matches!(event, UploadTransferEvent::Advanced { .. })
                && !started.swap(true, Ordering::AcqRel)
            {
                fault("temp-streaming");
            }
            inner.as_ref().is_none_or(|inner| inner(event))
        }))
    };
    if let Err(error) = ops.deposit_upload(local, &temp, streaming_observer, cancellation) {
        remove_temp(ops, root, context, intent, &temp, &error.to_string());
        return Err(naming_final(error, &temp, final_path));
    }
    fault("temp-verified");

    // The last look before publishing.
    let state = match ops.deposit_state(final_path) {
        Ok(state) => state,
        Err(error) => {
            remove_temp(ops, root, context, intent, &temp, &error.to_string());
            return Err(error);
        }
    };
    if !destination_matches(state, expected) {
        let detail = format!(
            "{final_path:?} is not what this run planned against (someone else wrote it, or it \
             became a folder); the verified copy was not published"
        );
        remove_temp(ops, root, context, intent, &temp, &detail);
        return Ok(Deposited::DestinationChanged(detail));
    }

    match state {
        RemotePathState::Absent => publish_new(
            ops, root, context, intent, local, &temp, temp_name, final_name,
        ),
        _ => {
            // The gap between the two renames is one round trip; nothing checks cancellation in
            // it, so a cancelled run finishes the swap it started.
            if let Err(error) = cancellation.check() {
                remove_temp(ops, root, context, intent, &temp, "the run was cancelled");
                return Err(error);
            }
            replace(
                ops,
                root,
                context,
                intent,
                local,
                (&temp, temp_name),
                (&backup, backup_name),
                final_name,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_new<O: DepositOps>(
    ops: &O,
    root: &RemoteRoot,
    context: &DepositContext,
    intent: &Intent,
    local: &LocalEntry,
    temp: &str,
    _temp_name: &str,
    final_name: &str,
) -> Result<Deposited> {
    let final_path = intent.final_path.as_str();
    match ops.deposit_rename(root, temp, final_name, true) {
        Ok(state) => {
            fault("published");
            check_published(context, final_path, local, state)?;
            context.journal.resolve(intent);
            context.published.fetch_add(1, Ordering::AcqRel);
            Ok(Deposited::New)
        }
        Err(error) => {
            // Taken meanwhile: DSM's Rename refuses an existing name, which is the point.
            if error.api_code() == Some(414) {
                let detail = format!(
                    "{final_path:?} appeared while the upload was in flight; it was kept and the \
                     verified copy was not published"
                );
                remove_temp(ops, root, context, intent, temp, &detail);
                return Ok(Deposited::DestinationChanged(detail));
            }
            // An answer that was lost: settle it from what the NAS now holds.
            match (ops.deposit_state(temp), ops.deposit_state(final_path)) {
                (Ok(RemotePathState::Absent), Ok(state @ RemotePathState::File { .. })) => {
                    check_published(context, final_path, local, Some(state))?;
                    context.journal.resolve(intent);
                    context.published.fetch_add(1, Ordering::AcqRel);
                    Ok(Deposited::New)
                }
                _ => {
                    remove_temp(ops, root, context, intent, temp, &error.to_string());
                    Err(naming_final(error, temp, final_path))
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn replace<O: DepositOps>(
    ops: &O,
    root: &RemoteRoot,
    context: &DepositContext,
    intent: &Intent,
    local: &LocalEntry,
    (temp, _temp_name): (&str, &str),
    (backup, backup_name): (&str, &str),
    final_name: &str,
) -> Result<Deposited> {
    let final_path = intent.final_path.as_str();
    // 1. The previous version moves aside.
    if let Err(error) = ops.deposit_rename(root, final_path, backup_name, false) {
        match (ops.deposit_state(final_path), ops.deposit_state(backup)) {
            // It happened; only the answer was lost.
            (Ok(RemotePathState::Absent), Ok(RemotePathState::File { .. })) => {}
            // It did not happen: nothing moved, the temporary copy goes.
            (Ok(RemotePathState::File { .. }), Ok(RemotePathState::Absent)) => {
                remove_temp(ops, root, context, intent, temp, &error.to_string());
                return Err(error);
            }
            _ => {
                context.issue(DepositIssue::warning(
                    "deposit-recovery-failed",
                    final_path,
                    format!(
                        "moving the previous version aside failed ({error}) and what the NAS now \
                         holds could not be read; the next run's recovery settles it"
                    ),
                ));
                return Err(error);
            }
        }
    }
    fault("backup-renamed");
    // 2. The verified copy takes the name.
    let published = match ops.deposit_rename(root, temp, final_name, true) {
        Ok(state) => Some(state),
        Err(error) => match (ops.deposit_state(temp), ops.deposit_state(final_path)) {
            (Ok(RemotePathState::Absent), Ok(state @ RemotePathState::File { .. })) => {
                Some(Some(state))
            }
            (_, Ok(RemotePathState::Absent)) => {
                // Not published: the previous version goes back, the copy goes.
                rollback(ops, root, context, intent, backup, final_name, &error);
                remove_temp(ops, root, context, intent, temp, &error.to_string());
                return Err(naming_final(error, temp, final_path));
            }
            _ => {
                // Something else took the name inside the gap: it is kept, and so is the previous
                // version, by name.
                context.issue(DepositIssue::warning(
                    "deposit-backup-kept",
                    backup,
                    format!(
                        "{final_path:?} was taken by another writer between the two renames of \
                         its replacement; the previous version is kept here and the other \
                         writer's file is left as it is"
                    ),
                ));
                remove_temp(ops, root, context, intent, temp, &error.to_string());
                return Ok(Deposited::DestinationChanged(format!(
                    "{final_path:?} was taken by another writer while it was being replaced; \
                     the previous version is kept as {backup:?}"
                )));
            }
        },
    };
    fault("published");
    check_published(context, final_path, local, published.flatten())?;
    context.replaced.fetch_add(1, Ordering::AcqRel);
    // 3. The previous version goes.
    match ops.deposit_delete(root, backup) {
        Ok(()) => {
            fault("backup-deleted");
            context.journal.resolve(intent);
        }
        Err(error) => context.issue(DepositIssue::warning(
            "deposit-cleanup-failed",
            backup,
            format!(
                "the replacement of {final_path:?} is published, but its previous version could \
                 not be deleted ({error}); the next run's recovery deletes it"
            ),
        )),
    }
    Ok(Deposited::Replaced)
}

/// A refusal DSM gave the temporary name is a refusal of the file: named by its final path, so
/// the deferral, the breaker's sample and the operator all see the path they know. (A name too
/// long for the volume, for one, is refused only when the short temporary name is renamed.)
fn naming_final(error: Error, temp: &str, final_path: &str) -> Error {
    match error {
        Error::RemoteRefused {
            operation,
            path,
            code,
            description,
        } if path == temp => Error::RemoteRefused {
            operation,
            path: final_path.to_owned(),
            code,
            description,
        },
        Error::ContentVerificationFailed(path) if path == temp => {
            Error::ContentVerificationFailed(final_path.to_owned())
        }
        // A failure of the whole run (a full volume, a quota) names the file too.
        Error::Api {
            api,
            operation,
            code,
            description,
            details,
        } => Error::Api {
            api,
            operation,
            code,
            description: description.replace(temp, final_path),
            details,
        },
        Error::Message(message) => Error::Message(message.replace(temp, final_path)),
        other => other,
    }
}

/// Put a moved-aside previous version back under its final name.
fn rollback<O: DepositOps>(
    ops: &O,
    root: &RemoteRoot,
    context: &DepositContext,
    intent: &Intent,
    backup: &str,
    final_name: &str,
    cause: &Error,
) {
    match ops.deposit_rename(root, backup, final_name, true) {
        Ok(_) => context.issue(DepositIssue::warning(
            "deposit-rolled-back",
            &intent.final_path,
            format!("publishing the replacement failed ({cause}); the previous version is back"),
        )),
        Err(error) => context.issue(DepositIssue::warning(
            "deposit-recovery-failed",
            backup,
            format!(
                "publishing the replacement of {:?} failed ({cause}) and putting the previous \
                 version back failed too ({error}); it is kept under this name and the next \
                 run's recovery restores it",
                intent.final_path
            ),
        )),
    }
}

/// The published file must be the one verified: its length, and the time the upload sent.
fn check_published(
    context: &DepositContext,
    final_path: &str,
    local: &LocalEntry,
    state: Option<RemotePathState>,
) -> Result<()> {
    match state {
        Some(RemotePathState::File {
            size,
            mtime_seconds,
        }) => {
            if size != local.size {
                return Err(Error::ContentVerificationFailed(final_path.to_owned()));
            }
            if let Some(mtime) = mtime_seconds
                && !crate::plan::mtimes_agree(local.mtime_ms, mtime)
            {
                context.issue(DepositIssue::warning(
                    "deposit-mtime-not-kept",
                    final_path,
                    format!(
                        "the published file's modification time is {mtime}, not the one the \
                         upload sent; DSM's Rename did not keep it, so a metadata comparison \
                         will see this file as changed"
                    ),
                ));
            }
            Ok(())
        }
        // Verified by the caller's own probe when the rename answer was lost.
        None => Ok(()),
        Some(_) => Err(Error::ContentVerificationFailed(final_path.to_owned())),
    }
}

/// Remove a deposit's temporary copy after it was not published. Resolves the journal entry
/// when the NAS confirms the copy is gone; otherwise names the path for the next run's recovery.
fn remove_temp<O: DepositOps>(
    ops: &O,
    root: &RemoteRoot,
    context: &DepositContext,
    intent: &Intent,
    temp: &str,
    cause: &str,
) {
    match ops.deposit_delete(root, temp) {
        Ok(()) => context.journal.resolve(intent),
        Err(error) => context.issue(DepositIssue::warning(
            "deposit-cleanup-failed",
            temp,
            format!(
                "the unpublished temporary copy of {:?} could not be removed ({error}) after \
                 {cause}; the next run's recovery removes it",
                intent.final_path
            ),
        )),
    }
}

// ------------------------------------------------------------------------------------------------
// Recovery
// ------------------------------------------------------------------------------------------------

/// What [`begin`] hands a run.
pub struct Begun {
    /// What the run's execution deposits through.
    pub deposit: crate::sync::Deposit,
    /// The journal, for [`end`]; `None` for a direct deposit.
    pub context: Option<std::sync::Arc<DepositContext>>,
    /// Every reserved NAS name the journal accounts for after recovery, for
    /// [`unjournaled_reserved`].
    pub journalled: BTreeSet<String>,
}

/// Start a run's deposit: for [`DepositMode::Atomic`], claim the journal in `journal_dir` (or
/// [`default_journal_dir`]), settle what an interrupted run left, and start this run's journal;
/// for [`DepositMode::Direct`], name the run `deposit-not-atomic`.
///
/// A journal that cannot be claimed fails the run before anything is listed or written: a
/// deposit it does not record could be left on the NAS for no recovery to find.
pub fn begin(
    mode: DepositMode,
    journal_dir: Option<&Path>,
    owner: &DepositOwner,
    client: &crate::api::ApiClient,
    root: &RemoteRoot,
    cancellation: &CancellationToken,
    report: &mut DepositReport,
) -> Result<Begun> {
    if mode == DepositMode::Direct {
        report.issues.push(DepositIssue::warning(
            "deposit-not-atomic",
            root.as_str(),
            "deposit = \"direct\": files are uploaded straight to their final names, so a run \
             killed mid-upload can leave a partial or torn file there",
        ));
        return Ok(Begun {
            deposit: crate::sync::Deposit::Direct,
            context: None,
            journalled: BTreeSet::new(),
        });
    }
    let directory = match journal_dir.map(Path::to_owned).or_else(default_journal_dir) {
        Some(directory) => directory,
        None => {
            return Err(journal_error(
                "deposit-journal-unavailable",
                Path::new(""),
                "there is no per-user state directory (HOME or LOCALAPPDATA is not set); give \
                 the journal a directory (--deposit-journal)",
            ));
        }
    };
    let (journal, prior) = DepositJournal::open(&directory, owner, root.as_str())?;
    let carried = recover(client, root, owner, prior, cancellation, report);
    let journalled = journalled_names(owner, &carried);
    journal.begin(carried)?;
    let context = std::sync::Arc::new(DepositContext::new(journal));
    Ok(Begun {
        deposit: crate::sync::Deposit::Atomic(std::sync::Arc::clone(&context)),
        context: Some(context),
        journalled,
    })
}

/// End a run's deposit: close the journal (settled deposits leave it, unsettled ones stay for
/// the next run) and complete `report`. A journal that cannot be closed is named; the next run
/// then re-checks every deposit it names.
pub fn end(context: Option<std::sync::Arc<DepositContext>>, report: &mut DepositReport) {
    let Some(context) = context else {
        return;
    };
    if let Err(error) = context.finish(report) {
        report.issues.push(DepositIssue::warning(
            "deposit-recovery-failed",
            &context.journal.path().display().to_string(),
            format!(
                "the deposit journal could not be closed ({error}); the next run re-checks every \
                 deposit it names"
            ),
        ));
    }
}

/// The journal recovery (`recover`) against the File Station client: what a run calls before it
/// lists the NAS.
pub fn recover_with(
    client: &crate::api::ApiClient,
    root: &RemoteRoot,
    owner: &DepositOwner,
    prior: PriorJournal,
    cancellation: &CancellationToken,
    report: &mut DepositReport,
) -> Vec<Intent> {
    recover(client, root, owner, prior, cancellation, report)
}

/// What recovery found for one prior deposit.
struct Found {
    temp: bool,
    backup: bool,
    final_state: RemotePathState,
}

/// Settle what earlier runs left: every journalled deposit is brought to "old version or new
/// version under the final name, nothing else", or kept and named. Returns the deposits that
/// could not be settled, for this run's journal to carry.
pub(crate) fn recover<O: DepositOps>(
    ops: &O,
    root: &RemoteRoot,
    owner: &DepositOwner,
    prior: PriorJournal,
    cancellation: &CancellationToken,
    report: &mut DepositReport,
) -> Vec<Intent> {
    report.issues.extend(prior.issues);
    let mut carried = Vec::new();
    // One listing per folder answers "is the temporary copy or the previous version there" for
    // every deposit in it; a folder that cannot be listed completely is asked name by name.
    let mut listings: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
    for intent in prior.intents {
        if cancellation.is_cancelled() {
            carried.push(intent);
            continue;
        }
        match recover_one(
            ops,
            root,
            owner,
            &intent,
            &mut listings,
            cancellation,
            prior.started,
        ) {
            Ok((settled, issue)) => {
                let acted = issue.is_some();
                if issue
                    .as_ref()
                    .is_some_and(|issue| issue.code == "deposit-copy-kept")
                {
                    report.suspect.insert(intent.final_path.clone());
                }
                if let Some(issue) = issue {
                    report.issues.push(issue);
                }
                if settled {
                    // Counted when recovery acted; a deposit that left nothing is just settled.
                    if acted {
                        report.recovered += 1;
                    }
                } else {
                    carried.push(intent);
                }
            }
            Err(error) => {
                report.issues.push(DepositIssue::warning(
                    "deposit-recovery-failed",
                    &intent.final_path,
                    format!(
                        "an earlier run's deposit could not be settled ({error}); it is kept for \
                         the next run"
                    ),
                ));
                carried.push(intent);
            }
        }
    }
    carried
}

fn present<O: DepositOps>(
    ops: &O,
    listings: &mut BTreeMap<String, Option<BTreeSet<String>>>,
    path: &str,
    cancellation: &CancellationToken,
) -> Result<bool> {
    let (parent, name) = parent_and_name(path)?;
    let listing = listings
        .entry(parent.to_owned())
        .or_insert_with(|| ops.deposit_list(parent, cancellation));
    match listing {
        Some(names) => Ok(names.contains(name)),
        None => Ok(!matches!(ops.deposit_state(path)?, RemotePathState::Absent)),
    }
}

/// `Ok((settled, issue))`: whether nothing of the deposit is left but its published file.
#[allow(clippy::too_many_arguments)]
fn recover_one<O: DepositOps>(
    ops: &O,
    root: &RemoteRoot,
    owner: &DepositOwner,
    intent: &Intent,
    listings: &mut BTreeMap<String, Option<BTreeSet<String>>>,
    cancellation: &CancellationToken,
    journal_started: Option<i64>,
) -> Result<(bool, Option<DepositIssue>)> {
    let final_path = intent.final_path.as_str();
    if !root.contains_child(final_path) {
        return Ok((
            false,
            Some(DepositIssue::warning(
                "deposit-recovery-failed",
                final_path,
                "the journalled path is outside this profile's destination; left alone",
            )),
        ));
    }
    let (_, final_name) = parent_and_name(final_path)?;
    match intent.kind {
        IntentKind::Upload => {
            let temp = intent.temp_path(owner)?;
            let backup = intent.backup_path(owner)?;
            let temp_present = present(ops, listings, &temp, cancellation)?;
            let backup_present = present(ops, listings, &backup, cancellation)?;
            // The final name only matters when a previous version was moved aside; a deposit
            // that never started, or finished cleanly, costs no request beyond the listing.
            let final_state = if backup_present && present(ops, listings, final_path, cancellation)?
            {
                ops.deposit_state(final_path)?
            } else {
                RemotePathState::Absent
            };
            let found = Found {
                temp: temp_present,
                backup: backup_present,
                final_state,
            };
            let mut issue = None;
            if found.backup {
                match found.final_state {
                    RemotePathState::Absent => {
                        ops.deposit_rename(root, &backup, final_name, true)?;
                        fault("recovery-rolled-back");
                        issue = Some(DepositIssue::warning(
                            "deposit-rolled-back",
                            final_path,
                            format!(
                                "an earlier run was interrupted while replacing it; the previous \
                                 version was restored from {backup:?}"
                            ),
                        ));
                    }
                    RemotePathState::File {
                        size,
                        mtime_seconds,
                    } => {
                        let is_new = size == intent.size
                            && mtime_seconds.is_none_or(|mtime| {
                                crate::plan::mtimes_agree(intent.mtime_ms, mtime)
                            })
                            && match intent.content_md5() {
                                Some(md5) => {
                                    ops.deposit_md5_matches(final_path, size, md5, cancellation)?
                                }
                                None => true,
                            };
                        if is_new {
                            ops.deposit_delete(root, &backup)?;
                            issue = Some(DepositIssue::info(
                                "deposit-completed",
                                final_path,
                                format!(
                                    "an earlier run published the replacement and was interrupted \
                                     before deleting the previous version; {backup:?} was deleted"
                                ),
                            ));
                        } else {
                            if found.temp {
                                ops.deposit_delete(root, &temp)?;
                            }
                            return Ok((
                                false,
                                Some(DepositIssue::warning(
                                    "deposit-backup-kept",
                                    &backup,
                                    format!(
                                        "an earlier run was interrupted while replacing \
                                         {final_path:?}, and that name now holds a file this tool \
                                         did not publish; both are kept: remove this previous \
                                         version by hand once you know which one you want"
                                    ),
                                )),
                            ));
                        }
                    }
                    RemotePathState::Directory => {
                        if found.temp {
                            ops.deposit_delete(root, &temp)?;
                        }
                        return Ok((
                            false,
                            Some(DepositIssue::warning(
                                "deposit-backup-kept",
                                &backup,
                                format!(
                                    "an earlier run was interrupted while replacing {final_path:?}, \
                                     and that name is now a folder; the previous version is kept"
                                ),
                            )),
                        ));
                    }
                }
            }
            if found.temp {
                ops.deposit_delete(root, &temp)?;
                if issue.is_none() {
                    issue = Some(DepositIssue::info(
                        "deposit-temp-removed",
                        &temp,
                        format!(
                            "an earlier run was interrupted before publishing {final_path:?}; its \
                             unpublished temporary copy was removed"
                        ),
                    ));
                }
            }
            Ok((true, issue))
        }
        IntentKind::Copy => {
            if !present(ops, listings, final_path, cancellation)? {
                return Ok((true, None));
            }
            let RemotePathState::File { size, .. } = ops.deposit_state(final_path)? else {
                return Ok((true, None));
            };
            let complete = size == intent.size
                && match intent.content_md5() {
                    Some(md5) => ops.deposit_md5_matches(final_path, size, md5, cancellation)?,
                    None => true,
                };
            if complete {
                return Ok((true, None));
            }
            // Not provably the interrupted copy: time has passed, and someone else may have
            // written the name since. It is kept and named, and never adopted: this run uploads
            // over it whatever its comparison says (`DepositReport::suspect`), through an atomic
            // deposit like any other overwrite.
            let _ = journal_started;
            Ok((
                true,
                Some(DepositIssue::warning(
                    "deposit-copy-kept",
                    final_path,
                    format!(
                        "an earlier run's server-side copy to it was interrupted; it holds {size} \
                         bytes where the copy would hold {}, so it is kept and this run's \
                         comparison replaces it",
                        intent.size
                    ),
                )),
            ))
        }
    }
}

/// Reserved names a listing found that this owner's journal does not account for: never touched,
/// always named. `entries` are NAS paths relative to `root`.
pub fn unjournaled_reserved<'a>(
    root: &RemoteRoot,
    entries: impl IntoIterator<Item = &'a str>,
    owner: &DepositOwner,
    journalled: &BTreeSet<String>,
) -> Vec<DepositIssue> {
    entries
        .into_iter()
        .filter(|relative| {
            relative
                .rsplit('/')
                .next()
                .is_some_and(crate::path::is_deposit_reserved_name)
        })
        .filter_map(|relative| {
            let path = format!("{}/{relative}", root.as_str());
            if journalled.contains(&path) {
                return None;
            }
            let name = relative.rsplit('/').next().unwrap_or(relative);
            let ours = name.contains(&format!("-{}-", owner.as_str()));
            Some(DepositIssue::warning(
                "deposit-orphan-unjournaled",
                &path,
                if ours {
                    "a temporary or previous-version file of this profile that no journal \
                     accounts for (the journal was lost or moved); it was left alone: check its \
                     content and remove it by hand"
                } else {
                    "a temporary or previous-version file another profile, installation or \
                     account left; it was left alone"
                },
            ))
        })
        .collect()
}

/// Every reserved NAS path this journal's unsettled deposits may have left, for
/// [`unjournaled_reserved`].
pub fn journalled_names(owner: &DepositOwner, intents: &[Intent]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for intent in intents {
        if let Ok(temp) = intent.temp_path(owner) {
            names.insert(temp);
        }
        if let Ok(backup) = intent.backup_path(owner) {
            names.insert(backup);
        }
    }
    names
}

#[cfg(test)]
mod tests;
