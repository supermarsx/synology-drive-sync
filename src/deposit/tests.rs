//! The deposit protocol, its journal and its recovery against an in-memory NAS whose Rename
//! either refuses a taken name (DSM's documented answer) or replaces a file (rename(2)), with a
//! record of what a reader could have seen under every name after every request.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use super::*;
use crate::local::EntryKind;

const ROOT: &str = "/team/dst";

/// What the fake does to one request.
#[derive(Clone, Debug, PartialEq)]
enum Fault {
    /// Refuse it with this DSM code, doing nothing.
    Refuse(i64),
    /// Do it, then lose the answer (a transport failure after DSM acted).
    LoseAnswer,
    /// Fail in transport without doing it.
    Drop,
}

#[derive(Default)]
struct FakeNas {
    files: Mutex<BTreeMap<String, (Vec<u8>, i64)>>,
    /// Upload sources by local relative path.
    sources: BTreeMap<String, Vec<u8>>,
    rename_replaces: bool,
    /// `(operation, path fragment)` -> fault, taken once.
    faults: Mutex<Vec<(&'static str, String, Fault)>>,
    /// Content of each watched path after every request that changed it.
    watched: Mutex<BTreeMap<String, Vec<Option<Vec<u8>>>>>,
    requests: Mutex<Vec<String>>,
}

impl FakeNas {
    fn with_file(self, path: &str, content: &[u8], mtime: i64) -> Self {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), (content.to_vec(), mtime));
        self
    }

    fn with_source(mut self, relative: &str, content: &[u8]) -> Self {
        self.sources.insert(relative.to_owned(), content.to_vec());
        self
    }

    fn watch(&self, path: &str) {
        let now = self.content(path);
        self.watched
            .lock()
            .unwrap()
            .insert(path.to_owned(), vec![now]);
    }

    fn fail(&self, operation: &'static str, fragment: &str, fault: Fault) {
        self.faults
            .lock()
            .unwrap()
            .push((operation, fragment.to_owned(), fault));
    }

    fn take_fault(&self, operation: &str, path: &str) -> Option<Fault> {
        let mut faults = self.faults.lock().unwrap();
        let index = faults
            .iter()
            .position(|(op, fragment, _)| *op == operation && path.contains(fragment.as_str()))?;
        Some(faults.remove(index).2)
    }

    fn content(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).map(|(c, _)| c.clone())
    }

    fn names(&self) -> BTreeSet<String> {
        self.files.lock().unwrap().keys().cloned().collect()
    }

    fn record(&self, operation: &str) {
        self.requests.lock().unwrap().push(operation.to_owned());
        let files = self.files.lock().unwrap();
        for (path, history) in self.watched.lock().unwrap().iter_mut() {
            let now = files.get(path).map(|(c, _)| c.clone());
            if history.last() != Some(&now) {
                history.push(now);
            }
        }
    }

    fn history(&self, path: &str) -> Vec<Option<Vec<u8>>> {
        self.watched
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    fn refused(code: i64, path: &str) -> Error {
        Error::RemoteRefused {
            operation: "test".to_owned(),
            path: path.to_owned(),
            code,
            description: String::new(),
        }
    }

    fn dropped() -> Error {
        Error::Message("connection reset".to_owned())
    }
}

impl DepositOps for FakeNas {
    fn deposit_upload(
        &self,
        local: &LocalEntry,
        remote_path: &str,
        observer: Option<UploadObserver>,
        _cancellation: &CancellationToken,
    ) -> Result<()> {
        if let Some(observer) = observer {
            observer(UploadTransferEvent::Advanced { bytes: 1 });
        }
        let fault = self.take_fault("upload", remote_path);
        let result = match fault {
            Some(Fault::Refuse(code)) => Err(Self::refused(code, remote_path)),
            Some(Fault::Drop) => {
                // A torn temporary copy: half the bytes, the way a dropped link leaves one.
                let source = &self.sources[&local.relative];
                self.files.lock().unwrap().insert(
                    remote_path.to_owned(),
                    (source[..source.len() / 2].to_vec(), 0),
                );
                Err(Self::dropped())
            }
            _ => {
                self.files.lock().unwrap().insert(
                    remote_path.to_owned(),
                    (
                        self.sources[&local.relative].clone(),
                        local.mtime_ms.div_euclid(1000),
                    ),
                );
                Ok(())
            }
        };
        self.record("upload");
        result
    }

    fn deposit_state(&self, remote_path: &str) -> Result<RemotePathState> {
        self.record("getinfo");
        Ok(match self.files.lock().unwrap().get(remote_path) {
            None => RemotePathState::Absent,
            Some((content, mtime)) => RemotePathState::File {
                size: content.len() as u64,
                mtime_seconds: Some(*mtime),
            },
        })
    }

    fn deposit_rename(
        &self,
        root: &RemoteRoot,
        remote_path: &str,
        new_name: &str,
        verify: bool,
    ) -> Result<Option<RemotePathState>> {
        assert!(root.contains_child(remote_path), "{remote_path}");
        let (parent, _) = parent_and_name(remote_path)?;
        let destination = format!("{parent}/{new_name}");
        let fault = self.take_fault("rename", remote_path);
        if let Some(Fault::Refuse(code)) = fault {
            self.record("rename");
            return Err(Self::refused(code, remote_path));
        }
        if fault == Some(Fault::Drop) {
            self.record("rename");
            return Err(Self::dropped());
        }
        {
            let mut files = self.files.lock().unwrap();
            if files.contains_key(&destination) && !self.rename_replaces {
                drop(files);
                self.record("rename");
                return Err(Self::refused(414, remote_path));
            }
            let Some(entry) = files.remove(remote_path) else {
                drop(files);
                self.record("rename");
                return Err(Self::refused(408, remote_path));
            };
            files.insert(destination.clone(), entry);
        }
        self.record("rename");
        if fault == Some(Fault::LoseAnswer) {
            return Err(Self::dropped());
        }
        if verify {
            self.deposit_state(&destination).map(Some)
        } else {
            Ok(None)
        }
    }

    fn deposit_delete(&self, root: &RemoteRoot, remote_path: &str) -> Result<()> {
        assert!(root.contains_child(remote_path), "{remote_path}");
        if let Some(Fault::Refuse(code)) = self.take_fault("delete", remote_path) {
            self.record("delete");
            return Err(Self::refused(code, remote_path));
        }
        self.files.lock().unwrap().remove(remote_path);
        self.record("delete");
        Ok(())
    }

    fn deposit_list(
        &self,
        folder: &str,
        _cancellation: &CancellationToken,
    ) -> Option<BTreeSet<String>> {
        self.record("list");
        let prefix = format!("{folder}/");
        Some(
            self.files
                .lock()
                .unwrap()
                .keys()
                .filter_map(|path| path.strip_prefix(&prefix))
                .filter(|rest| !rest.contains('/'))
                .map(str::to_owned)
                .collect(),
        )
    }

    fn deposit_md5_matches(
        &self,
        remote_path: &str,
        size: u64,
        md5: ContentMd5,
        _cancellation: &CancellationToken,
    ) -> Result<bool> {
        self.record("md5");
        Ok(match self.files.lock().unwrap().get(remote_path) {
            Some((content, _)) => {
                content.len() as u64 == size && ContentMd5::from_content(content).md5_matches(&md5)
            }
            None => false,
        })
    }
}

fn root() -> RemoteRoot {
    RemoteRoot::parse(ROOT).unwrap()
}

fn local(relative: &str, content: &[u8], mtime_ms: i64) -> LocalEntry {
    LocalEntry {
        relative: relative.to_owned(),
        full_path: PathBuf::from(relative),
        kind: EntryKind::File,
        size: content.len() as u64,
        mtime_ms,
        identity: Default::default(),
        times: Default::default(),
        content_md5: Some(ContentMd5::from_content(content)),
    }
}

fn snapshot(content: &[u8], mtime: i64) -> RemoteSnapshot {
    RemoteSnapshot {
        kind: EntryKind::File,
        size: content.len() as u64,
        mtime_seconds: mtime,
        content_md5: None,
        require_mtime: true,
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sdsync-deposit-{tag}-{}-{nanos}",
            std::process::id()
        ));
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn owner() -> DepositOwner {
    DepositOwner::derive("p", ROOT, "https://nas", "me")
}

/// A started journal in a scratch directory, and the context a run deposits through.
fn context(scratch: &Scratch) -> DepositContext {
    let (journal, prior) = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap();
    assert!(prior.intents.is_empty());
    journal.begin(Vec::new()).unwrap();
    DepositContext::new(journal)
}

fn declare(context: &DepositContext, local: &LocalEntry, path: &str) -> Intent {
    context
        .journal
        .declare(&[NewIntent::upload(local, path)])
        .unwrap()
        .remove(0)
}

fn deposit(
    nas: &FakeNas,
    context: &DepositContext,
    local: &LocalEntry,
    path: &str,
    expected: Option<&RemoteSnapshot>,
) -> Result<Deposited> {
    let intent = declare(context, local, path);
    deposit_file(
        nas,
        &root(),
        context,
        &intent,
        local,
        expected,
        None,
        &CancellationToken::default(),
    )
}

fn issues(context: &DepositContext) -> Vec<&'static str> {
    context
        .issues
        .lock()
        .unwrap()
        .iter()
        .map(|issue| issue.code)
        .collect()
}

fn reserved(nas: &FakeNas) -> Vec<String> {
    nas.names()
        .into_iter()
        .filter(|path| {
            path.rsplit('/')
                .next()
                .is_some_and(crate::path::is_deposit_reserved_name)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The protocol
// ---------------------------------------------------------------------------------------------

#[test]
fn a_new_file_appears_under_its_name_only_once_it_is_whole() {
    let scratch = Scratch::new("new");
    let context = context(&scratch);
    let nas = FakeNas::default().with_source("a.txt", b"new content");
    let path = format!("{ROOT}/a.txt");
    nas.watch(&path);

    let outcome = deposit(
        &nas,
        &context,
        &local("a.txt", b"new content", 5_000),
        &path,
        None,
    );

    assert_eq!(outcome.unwrap(), Deposited::New);
    assert_eq!(nas.content(&path).as_deref(), Some(&b"new content"[..]));
    assert_eq!(
        nas.history(&path),
        vec![None, Some(b"new content".to_vec())]
    );
    assert!(reserved(&nas).is_empty(), "{:?}", nas.names());
    assert_eq!(context.journal.unsettled(), 0);
    assert!(issues(&context).is_empty());
}

#[test]
fn an_overwrite_shows_old_then_new_and_never_a_partial_file() {
    for replaces in [false, true] {
        let scratch = Scratch::new("overwrite");
        let context = context(&scratch);
        let path = format!("{ROOT}/a.txt");
        let nas = FakeNas {
            rename_replaces: replaces,
            ..FakeNas::default()
        }
        .with_file(&path, b"old", 1)
        .with_source("a.txt", b"brand new");
        nas.watch(&path);

        let outcome = deposit(
            &nas,
            &context,
            &local("a.txt", b"brand new", 9_000),
            &path,
            Some(&snapshot(b"old", 1)),
        );

        assert_eq!(outcome.unwrap(), Deposited::Replaced, "replaces={replaces}");
        // The previous version is moved aside before the new one takes the name: absent for the
        // one request between the two renames, never partial, never anything else.
        assert_eq!(
            nas.history(&path),
            vec![Some(b"old".to_vec()), None, Some(b"brand new".to_vec())],
            "replaces={replaces}"
        );
        assert!(reserved(&nas).is_empty(), "{:?}", nas.names());
        assert_eq!(context.journal.unsettled(), 0);
    }
}

#[test]
fn a_file_someone_created_meanwhile_is_kept_and_the_copy_is_not_published() {
    let scratch = Scratch::new("appeared");
    let context = context(&scratch);
    let path = format!("{ROOT}/a.txt");
    let nas = FakeNas::default()
        .with_file(&path, b"theirs", 3)
        .with_source("a.txt", b"mine");

    let outcome = deposit(&nas, &context, &local("a.txt", b"mine", 1_000), &path, None).unwrap();

    assert!(
        matches!(outcome, Deposited::DestinationChanged(_)),
        "{outcome:?}"
    );
    assert_eq!(nas.content(&path).as_deref(), Some(&b"theirs"[..]));
    assert!(reserved(&nas).is_empty(), "{:?}", nas.names());
    assert_eq!(context.journal.unsettled(), 0);
}

#[test]
fn a_destination_that_changed_since_planning_is_not_replaced() {
    let scratch = Scratch::new("changed");
    let context = context(&scratch);
    let path = format!("{ROOT}/a.txt");
    let nas = FakeNas::default()
        .with_file(&path, b"edited on the NAS", 7)
        .with_source("a.txt", b"mine");

    let outcome = deposit(
        &nas,
        &context,
        &local("a.txt", b"mine", 1_000),
        &path,
        Some(&snapshot(b"old", 1)),
    )
    .unwrap();

    assert!(matches!(outcome, Deposited::DestinationChanged(_)));
    assert_eq!(
        nas.content(&path).as_deref(),
        Some(&b"edited on the NAS"[..])
    );
    assert!(reserved(&nas).is_empty());
}

#[test]
fn a_name_taken_between_the_check_and_the_publishing_rename_is_kept() {
    let scratch = Scratch::new("race");
    let context = context(&scratch);
    let path = format!("{ROOT}/a.txt");
    let nas = FakeNas::default().with_source("a.txt", b"mine");
    // DSM refuses the publishing rename because the name was taken in the meantime.
    nas.fail("rename", "~sdsync-part-", Fault::Refuse(414));

    let outcome = deposit(&nas, &context, &local("a.txt", b"mine", 1_000), &path, None).unwrap();

    assert!(matches!(outcome, Deposited::DestinationChanged(_)));
    assert!(reserved(&nas).is_empty(), "{:?}", nas.names());
    assert_eq!(context.journal.unsettled(), 0);
}

#[test]
fn a_failed_upload_leaves_nothing_and_the_final_name_untouched() {
    let scratch = Scratch::new("upload-fails");
    let context = context(&scratch);
    let path = format!("{ROOT}/a.txt");
    let nas = FakeNas::default()
        .with_file(&path, b"old", 1)
        .with_source("a.txt", b"brand new");
    nas.watch(&path);
    nas.fail("upload", "~sdsync-part-", Fault::Drop);

    let error = deposit(
        &nas,
        &context,
        &local("a.txt", b"brand new", 9_000),
        &path,
        Some(&snapshot(b"old", 1)),
    )
    .unwrap_err();

    assert!(error.to_string().contains("connection reset"), "{error}");
    assert_eq!(nas.history(&path), vec![Some(b"old".to_vec())]);
    assert!(
        reserved(&nas).is_empty(),
        "the torn temporary copy is removed"
    );
    assert_eq!(context.journal.unsettled(), 0);
}

#[test]
fn a_publishing_rename_whose_answer_was_lost_is_settled_from_the_nas() {
    let scratch = Scratch::new("lost-answer");
    let context = context(&scratch);
    let path = format!("{ROOT}/a.txt");
    let nas = FakeNas::default().with_source("a.txt", b"mine");
    nas.fail("rename", "~sdsync-part-", Fault::LoseAnswer);

    let outcome = deposit(&nas, &context, &local("a.txt", b"mine", 1_000), &path, None);

    assert_eq!(outcome.unwrap(), Deposited::New);
    assert_eq!(nas.content(&path).as_deref(), Some(&b"mine"[..]));
    assert_eq!(context.journal.unsettled(), 0);
}

#[test]
fn a_replacement_that_cannot_take_the_name_puts_the_old_version_back() {
    let scratch = Scratch::new("rollback");
    let context = context(&scratch);
    let path = format!("{ROOT}/a.txt");
    let nas = FakeNas::default()
        .with_file(&path, b"old", 1)
        .with_source("a.txt", b"brand new");
    nas.watch(&path);
    nas.fail("rename", "~sdsync-part-", Fault::Refuse(900));

    let error = deposit(
        &nas,
        &context,
        &local("a.txt", b"brand new", 9_000),
        &path,
        Some(&snapshot(b"old", 1)),
    )
    .unwrap_err();

    assert_eq!(error.api_code(), Some(900));
    assert_eq!(
        nas.history(&path),
        vec![Some(b"old".to_vec()), None, Some(b"old".to_vec())]
    );
    assert!(reserved(&nas).is_empty(), "{:?}", nas.names());
    assert_eq!(issues(&context), vec!["deposit-rolled-back"]);
    assert_eq!(context.journal.unsettled(), 0);
}

#[test]
fn a_previous_version_that_cannot_be_deleted_is_left_for_recovery_by_name() {
    let scratch = Scratch::new("backup-stays");
    let context = context(&scratch);
    let path = format!("{ROOT}/a.txt");
    let nas = FakeNas::default()
        .with_file(&path, b"old", 1)
        .with_source("a.txt", b"brand new");
    nas.fail("delete", "~sdsync-prev-", Fault::Refuse(900));

    let outcome = deposit(
        &nas,
        &context,
        &local("a.txt", b"brand new", 9_000),
        &path,
        Some(&snapshot(b"old", 1)),
    );

    assert_eq!(outcome.unwrap(), Deposited::Replaced);
    assert_eq!(nas.content(&path).as_deref(), Some(&b"brand new"[..]));
    assert_eq!(issues(&context), vec!["deposit-cleanup-failed"]);
    assert_eq!(
        context.journal.unsettled(),
        1,
        "the journal keeps it for recovery"
    );
}

// ---------------------------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------------------------

fn prior_with(scratch: &Scratch, intents: &[NewIntent]) -> Vec<Intent> {
    let (journal, _) = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap();
    journal.begin(Vec::new()).unwrap();
    let declared = journal.declare(intents).unwrap();
    // Dropped without close: what a killed run leaves.
    drop(journal);
    declared
}

#[test]
fn a_settled_run_removes_its_journal_and_an_unsettled_one_carries_exactly_the_rest() {
    let scratch = Scratch::new("journal");
    let context = context(&scratch);
    let a = declare(&context, &local("a", b"a", 1_000), &format!("{ROOT}/a"));
    let b = declare(&context, &local("b", b"b", 1_000), &format!("{ROOT}/b"));
    context.journal.resolve(&a);
    assert_eq!(context.journal.close().unwrap(), 1);
    drop(context);

    let (journal, prior) = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap();
    assert_eq!(prior.intents, vec![b.clone()]);
    journal.begin(Vec::new()).unwrap();
    assert_eq!(journal.close().unwrap(), 0);
    assert!(!journal.path().exists(), "a settled journal is removed");
}

#[test]
fn a_killed_run_leaves_every_declaration_for_the_next_run() {
    let scratch = Scratch::new("killed");
    let declared = prior_with(
        &scratch,
        &[
            NewIntent::upload(&local("a", b"a", 1_000), &format!("{ROOT}/a")),
            NewIntent::upload(&local("b", b"b", 1_000), &format!("{ROOT}/b")),
        ],
    );
    let (_journal, prior) = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap();
    assert_eq!(prior.intents, declared);
}

#[test]
fn a_torn_last_record_is_ignored_by_name_and_a_torn_middle_one_refuses_the_run() {
    let scratch = Scratch::new("torn");
    prior_with(
        &scratch,
        &[NewIntent::upload(
            &local("a", b"a", 1_000),
            &format!("{ROOT}/a"),
        )],
    );
    let path = scratch.0.join(format!("{}.journal", owner().as_str()));
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("{\"run\":\"x\",\"seq\":");
    fs::write(&path, &text).unwrap();
    let (_journal, prior) = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap();
    assert_eq!(prior.intents.len(), 1);
    assert_eq!(prior.issues[0].code, "deposit-journal-torn");
    drop(_journal);

    text.push_str("\n{\"run\":\"y\"}\n");
    fs::write(&path, &text).unwrap();
    let error = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap_err();
    assert!(
        error.to_string().starts_with("deposit-journal-corrupt"),
        "{error}"
    );
}

#[test]
fn a_second_run_of_one_profile_is_refused_while_the_first_holds_the_journal() {
    let scratch = Scratch::new("locked");
    let (_first, _) = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap();
    let error = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap_err();
    assert!(
        error.to_string().starts_with("deposit-journal-locked"),
        "{error}"
    );
    // Another profile is another owner, with its own journal and lock.
    let other = DepositOwner::derive("q", ROOT, "https://nas", "me");
    assert_ne!(other, owner());
    DepositJournal::open(&scratch.0, &other, ROOT).unwrap();
}

#[cfg(unix)]
#[test]
fn a_journal_directory_others_may_write_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = Scratch::new("unsafe");
    fs::create_dir_all(&scratch.0).unwrap();
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o777)).unwrap();
    let error = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap_err();
    assert!(
        error.to_string().starts_with("deposit-journal-unsafe"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------------------------

fn recover_all(nas: &FakeNas, scratch: &Scratch) -> (Vec<Intent>, DepositReport) {
    let (_journal, prior) = DepositJournal::open(&scratch.0, &owner(), ROOT).unwrap();
    let mut report = DepositReport::default();
    let carried = recover(
        nas,
        &root(),
        &owner(),
        prior,
        &CancellationToken::default(),
        &mut report,
    );
    (carried, report)
}

fn codes(report: &DepositReport) -> Vec<&'static str> {
    report.issues.iter().map(|issue| issue.code).collect()
}

#[test]
fn recovery_removes_an_unpublished_temporary_copy() {
    let scratch = Scratch::new("rec-temp");
    let path = format!("{ROOT}/a.txt");
    let intent = prior_with(
        &scratch,
        &[NewIntent::upload(&local("a.txt", b"new", 1_000), &path)],
    )
    .remove(0);
    let nas = FakeNas::default().with_file(&path, b"old", 1).with_file(
        &intent.temp_path(&owner()).unwrap(),
        b"ne",
        0,
    );

    let (carried, report) = recover_all(&nas, &scratch);

    assert!(carried.is_empty());
    assert_eq!(codes(&report), vec!["deposit-temp-removed"]);
    assert_eq!(nas.content(&path).as_deref(), Some(&b"old"[..]));
    assert!(reserved(&nas).is_empty());
}

#[test]
fn recovery_puts_the_old_version_back_when_the_run_died_between_the_renames() {
    let scratch = Scratch::new("rec-gap");
    let path = format!("{ROOT}/a.txt");
    let intent = prior_with(
        &scratch,
        &[NewIntent::upload(&local("a.txt", b"new", 1_000), &path)],
    )
    .remove(0);
    let nas = FakeNas::default()
        .with_file(&intent.backup_path(&owner()).unwrap(), b"old", 1)
        .with_file(&intent.temp_path(&owner()).unwrap(), b"new", 1);

    let (carried, report) = recover_all(&nas, &scratch);

    assert!(carried.is_empty());
    assert_eq!(codes(&report), vec!["deposit-rolled-back"]);
    assert_eq!(nas.content(&path).as_deref(), Some(&b"old"[..]));
    assert!(reserved(&nas).is_empty(), "{:?}", nas.names());
}

#[test]
fn recovery_deletes_the_old_version_once_the_new_one_is_published() {
    let scratch = Scratch::new("rec-done");
    let path = format!("{ROOT}/a.txt");
    let intent = prior_with(
        &scratch,
        &[NewIntent::upload(&local("a.txt", b"new", 1_000), &path)],
    )
    .remove(0);
    let nas = FakeNas::default().with_file(&path, b"new", 1).with_file(
        &intent.backup_path(&owner()).unwrap(),
        b"old",
        1,
    );

    let (carried, report) = recover_all(&nas, &scratch);

    assert!(carried.is_empty());
    assert_eq!(codes(&report), vec!["deposit-completed"]);
    assert_eq!(nas.content(&path).as_deref(), Some(&b"new"[..]));
    assert!(reserved(&nas).is_empty());
}

#[test]
fn recovery_keeps_both_when_the_name_holds_someone_elses_file() {
    let scratch = Scratch::new("rec-foreign");
    let path = format!("{ROOT}/a.txt");
    let intent = prior_with(
        &scratch,
        &[NewIntent::upload(&local("a.txt", b"new", 1_000), &path)],
    )
    .remove(0);
    let backup = intent.backup_path(&owner()).unwrap();
    let nas = FakeNas::default()
        .with_file(&path, b"someone else's", 5)
        .with_file(&backup, b"old", 1);

    let (carried, report) = recover_all(&nas, &scratch);

    assert_eq!(carried, vec![intent], "kept, and named again next run");
    assert_eq!(codes(&report), vec!["deposit-backup-kept"]);
    assert_eq!(nas.content(&path).as_deref(), Some(&b"someone else's"[..]));
    assert_eq!(nas.content(&backup).as_deref(), Some(&b"old"[..]));
}

#[test]
fn recovery_keeps_an_interrupted_server_copy_it_cannot_prove_for_the_comparison_to_replace() {
    let scratch = Scratch::new("rec-copy");
    let path = format!("{ROOT}/moved/big.bin");
    prior_with(
        &scratch,
        &[NewIntent {
            final_path: path.clone(),
            kind: IntentKind::Copy,
            size: 10,
            md5: Some(ContentMd5::from_content(b"0123456789")),
            mtime_ms: 1_000,
        }],
    );
    let nas = FakeNas::default().with_file(&path, b"01234", 1);

    let (carried, report) = recover_all(&nas, &scratch);

    assert!(carried.is_empty());
    assert_eq!(codes(&report), vec!["deposit-copy-kept"]);
    assert_eq!(
        report.suspect,
        BTreeSet::from([path.clone()]),
        "kept, and uploaded over by this run, never adopted"
    );
    assert_eq!(nas.content(&path).as_deref(), Some(&b"01234"[..]));
}

#[test]
fn recovery_of_a_deposit_that_never_started_costs_one_listing_per_folder_and_changes_nothing() {
    let scratch = Scratch::new("rec-nothing");
    let entries = (0..50)
        .map(|n| NewIntent::upload(&local("x", b"x", 1_000), &format!("{ROOT}/d/{n}.txt")))
        .collect::<Vec<_>>();
    prior_with(&scratch, &entries);
    let nas = FakeNas::default().with_file(&format!("{ROOT}/d/1.txt"), b"old", 1);

    let (carried, report) = recover_all(&nas, &scratch);

    assert!(carried.is_empty());
    assert!(report.issues.is_empty());
    assert_eq!(report.recovered, 0, "nothing needed doing");
    let requests = nas.requests.lock().unwrap().clone();
    assert_eq!(
        requests.iter().filter(|r| *r == "list").count(),
        1,
        "{requests:?}"
    );
    assert_eq!(
        nas.content(&format!("{ROOT}/d/1.txt")).as_deref(),
        Some(&b"old"[..])
    );
}

#[test]
fn reserved_names_no_journal_accounts_for_are_named_and_never_touched() {
    let ours = format!("{ROOT}/d/~sdsync-part-{}-aaaaaaaaaaaa-3", owner().as_str());
    let theirs = format!("{ROOT}/d/~sdsync-prev-0123456789ab-aaaaaaaaaaaa-3");
    let relatives = [
        ours.trim_start_matches(&format!("{ROOT}/")).to_owned(),
        theirs.trim_start_matches(&format!("{ROOT}/")).to_owned(),
        "d/plain.txt".to_owned(),
    ];
    let found = unjournaled_reserved(
        &root(),
        relatives.iter().map(String::as_str),
        &owner(),
        &BTreeSet::new(),
    );
    assert_eq!(found.len(), 2);
    assert!(
        found
            .iter()
            .all(|issue| issue.code == "deposit-orphan-unjournaled")
    );
    assert!(
        found[0].cause.contains("of this profile"),
        "{}",
        found[0].cause
    );
    assert!(
        found[1].cause.contains("another profile"),
        "{}",
        found[1].cause
    );
    let journalled = BTreeSet::from([ours.clone()]);
    assert_eq!(
        unjournaled_reserved(
            &root(),
            relatives.iter().map(String::as_str),
            &owner(),
            &journalled
        )
        .len(),
        1
    );
}

#[test]
fn every_issue_code_is_in_the_published_vocabulary() {
    let source = include_str!("../deposit.rs");
    for code in source
        .split('"')
        .filter(|piece| piece.starts_with("deposit-") && !piece.contains(' '))
    {
        if code.starts_with("deposit-journal-")
            && ["unavailable", "locked", "unsafe", "corrupt"]
                .iter()
                .any(|suffix| code.ends_with(suffix))
        {
            continue; // run failures, not issues
        }
        assert!(ISSUE_CODES.contains(&code), "{code}");
    }
}
