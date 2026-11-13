//! The native watcher against real inotify: what each kind of change on a real tree reports, and
//! what the tree keeps watching afterwards.
//!
//! Linux only. Two tests need a lowered kernel limit and run only when the CI job that lowers it
//! says so (`SDSYNC_TEST_LOWERED_WATCH_LIMIT=1`, set after `sysctl fs.inotify.max_user_watches`);
//! everything else needs nothing beyond an ordinary user.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use synology_drive_sync::watch::{
    Change, EventSet, Filter, LimitPolicy, TreeWatch, WatchError, WatchOptions,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "sdsync-watch-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(base.join("source")).unwrap();
        fs::create_dir_all(base.join("outside")).unwrap();
        Self(base)
    }

    fn source(&self) -> PathBuf {
        self.0.join("source")
    }

    fn outside(&self) -> PathBuf {
        self.0.join("outside")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A mode-0000 directory a test made would otherwise stop the removal.
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.0)
            .status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn options(root: &Path, extra: &[&str], events: EventSet, limit: LimitPolicy) -> WatchOptions {
    WatchOptions {
        filter: Filter::build(
            root,
            extra.iter().map(|value| (*value).to_owned()).collect(),
        )
        .unwrap(),
        events,
        limit,
        setup_deadline: Duration::from_secs(20),
    }
}

fn arm(root: &Path) -> TreeWatch {
    TreeWatch::arm(
        root,
        options(root, &[], EventSet::all(), LimitPolicy::Unchecked),
    )
    .unwrap()
}

/// The controller's own event set: close_write, create, delete, move, attrib, modify.
fn controller_events() -> EventSet {
    EventSet::all()
}

/// The set the controller gave `inotifywait`: close_write, create, delete, move.
fn legacy_events() -> EventSet {
    EventSet {
        attrib: false,
        modify: false,
        ..EventSet::all()
    }
}

/// Changes until `done` holds for everything gathered so far, or five seconds pass.
fn gather(tree: &mut TreeWatch, done: impl Fn(&[Change]) -> bool) -> Vec<Change> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut changes = Vec::new();
    while Instant::now() < deadline && !done(&changes) {
        changes.extend(tree.wait(Duration::from_millis(100)).unwrap());
    }
    changes
}

/// Everything that arrives in `window`, for asserting that nothing does.
fn quiet(tree: &mut TreeWatch, window: Duration) -> Vec<Change> {
    let deadline = Instant::now() + window;
    let mut changes = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        changes.extend(
            tree.wait(remaining.min(Duration::from_millis(100)))
                .unwrap(),
        );
    }
    changes
}

fn has(changes: &[Change], entry: &str, kind: &str) -> bool {
    changes.iter().any(|change| {
        matches!(change, Change::Path { entry: e, kind: k, .. } if e == entry && *k == kind)
    })
}

fn entries(changes: &[Change]) -> Vec<String> {
    changes
        .iter()
        .filter_map(|change| match change {
            Change::Path { entry, kind, .. } => Some(format!("{kind}:{entry}")),
            Change::Overflow => None,
        })
        .collect()
}

#[test]
fn a_created_file_is_reported_with_its_directory() {
    let scratch = Scratch::new("create");
    let source = scratch.source();
    fs::create_dir(source.join("docs")).unwrap();
    let mut tree = arm(&source);
    assert_eq!(tree.watch_count(), 2);
    fs::write(source.join("docs/report.txt"), b"hello").unwrap();
    let changes = gather(&mut tree, |seen| {
        has(seen, "docs/report.txt", "close_write")
    });
    assert!(
        has(&changes, "docs/report.txt", "create"),
        "{:?}",
        entries(&changes)
    );
    assert!(changes.iter().any(|change| matches!(
        change,
        Change::Path { dir, entry, .. } if dir == "docs" && entry == "docs/report.txt"
    )));
}

#[test]
fn a_write_is_reported_when_the_file_is_closed() {
    use std::io::Write;
    let scratch = Scratch::new("close");
    let source = scratch.source();
    fs::write(source.join("ledger.csv"), b"a\n").unwrap();
    let mut tree = TreeWatch::arm(
        &source,
        options(&source, &[], legacy_events(), LimitPolicy::Unchecked),
    )
    .unwrap();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(source.join("ledger.csv"))
        .unwrap();
    file.write_all(b"b\n").unwrap();
    file.flush().unwrap();
    // Modified but still open: the inotifywait set has no `modify`, so nothing yet.
    assert!(quiet(&mut tree, Duration::from_millis(300)).is_empty());
    drop(file);
    let changes = gather(&mut tree, |seen| has(seen, "ledger.csv", "close_write"));
    assert!(
        has(&changes, "ledger.csv", "close_write"),
        "{:?}",
        entries(&changes)
    );
}

#[test]
fn renames_into_and_out_of_the_tree_are_reported_and_the_watch_set_follows() {
    let scratch = Scratch::new("rename");
    let source = scratch.source();
    let outside = scratch.outside();
    fs::create_dir_all(source.join("leaving/inner")).unwrap();
    fs::create_dir_all(outside.join("arriving/inner")).unwrap();
    fs::write(outside.join("arriving/inner/existing.txt"), b"x").unwrap();
    fs::write(outside.join("file.txt"), b"x").unwrap();
    let mut tree = arm(&source);
    assert_eq!(tree.watch_count(), 3);

    fs::rename(outside.join("file.txt"), source.join("file.txt")).unwrap();
    let changes = gather(&mut tree, |seen| has(seen, "file.txt", "moved_to"));
    assert!(
        has(&changes, "file.txt", "moved_to"),
        "{:?}",
        entries(&changes)
    );

    fs::rename(source.join("file.txt"), outside.join("file.txt")).unwrap();
    let changes = gather(&mut tree, |seen| has(seen, "file.txt", "moved_from"));
    assert!(
        has(&changes, "file.txt", "moved_from"),
        "{:?}",
        entries(&changes)
    );

    // A directory moved in is watched to its leaves, and what it brought is a change.
    fs::rename(outside.join("arriving"), source.join("arriving")).unwrap();
    let changes = gather(&mut tree, |seen| has(seen, "arriving/inner", "new_dir"));
    assert!(
        has(&changes, "arriving", "moved_to"),
        "{:?}",
        entries(&changes)
    );
    assert!(
        has(&changes, "arriving/inner", "new_dir"),
        "{:?}",
        entries(&changes)
    );
    assert!(tree.is_watched("arriving/inner"));
    assert_eq!(tree.watch_count(), 5);

    // A directory moved out stops being watched, and what happens in it afterwards is not news.
    fs::rename(source.join("leaving"), outside.join("leaving")).unwrap();
    let changes = gather(&mut tree, |seen| has(seen, "leaving", "moved_from"));
    assert!(
        has(&changes, "leaving", "moved_from"),
        "{:?}",
        entries(&changes)
    );
    assert_eq!(tree.watch_count(), 3);
    assert!(!tree.is_watched("leaving/inner"));
    fs::write(outside.join("leaving/inner/later.txt"), b"x").unwrap();
    let after = quiet(&mut tree, Duration::from_millis(300));
    assert!(after.is_empty(), "{:?}", entries(&after));
}

#[test]
fn a_directory_renamed_inside_the_tree_keeps_its_watches_under_the_new_path() {
    let scratch = Scratch::new("inner-rename");
    let source = scratch.source();
    fs::create_dir_all(source.join("before/child")).unwrap();
    let mut tree = arm(&source);
    fs::rename(source.join("before"), source.join("after")).unwrap();
    let changes = gather(&mut tree, |seen| has(seen, "after", "moved_to"));
    assert!(
        has(&changes, "before", "moved_from"),
        "{:?}",
        entries(&changes)
    );
    assert_eq!(tree.watch_count(), 3);
    assert!(tree.is_watched("after/child"));
    fs::write(source.join("after/child/note.txt"), b"x").unwrap();
    let changes = gather(&mut tree, |seen| {
        has(seen, "after/child/note.txt", "close_write")
    });
    assert!(
        has(&changes, "after/child/note.txt", "close_write"),
        "{:?}",
        entries(&changes)
    );
}

#[test]
fn mkdir_p_then_a_file_within_a_millisecond_is_never_missed() {
    let scratch = Scratch::new("race");
    let source = scratch.source();
    let mut tree = TreeWatch::arm(
        &source,
        options(&source, &[], controller_events(), LimitPolicy::Unchecked),
    )
    .unwrap();
    // Back to back, with nothing read in between: on an idle machine the file lands well inside
    // the millisecond, usually before the watcher has even seen `a`. Not asserted as a time --
    // a loaded runner stretches it -- because the claim holds whichever way the race goes.
    fs::create_dir_all(source.join("a/b/c")).unwrap();
    fs::write(source.join("a/b/c/f.txt"), b"x").unwrap();
    // Either the file's own event, or the walk that found c after its creation and reported it
    // whole: a scoped sync of c (or an ancestor) carries f.txt either way.
    let covered = |seen: &[Change]| {
        has(seen, "a/b/c/f.txt", "close_write")
            || seen.iter().any(|change| {
                matches!(change, Change::Path { dir, kind: "new_dir", .. } if dir == "a/b/c")
            })
    };
    let changes = gather(&mut tree, covered);
    assert!(covered(&changes), "{:?}", entries(&changes));
    assert!(tree.is_watched("a") && tree.is_watched("a/b") && tree.is_watched("a/b/c"));
    // And c is now watched for good.
    fs::write(source.join("a/b/c/g.txt"), b"x").unwrap();
    let changes = gather(&mut tree, |seen| has(seen, "a/b/c/g.txt", "close_write"));
    assert!(
        has(&changes, "a/b/c/g.txt", "close_write"),
        "{:?}",
        entries(&changes)
    );
}

#[test]
fn rm_rf_of_a_subtree_is_reported_and_releases_its_watches() {
    let scratch = Scratch::new("rmrf");
    let source = scratch.source();
    fs::create_dir_all(source.join("gone/x/y")).unwrap();
    fs::write(source.join("gone/x/y/file"), b"x").unwrap();
    let mut tree = arm(&source);
    assert_eq!(tree.watch_count(), 4);
    fs::remove_dir_all(source.join("gone")).unwrap();
    let changes = gather(&mut tree, |seen| has(seen, "gone", "delete"));
    assert!(has(&changes, "gone", "delete"), "{:?}", entries(&changes));
    let deadline = Instant::now() + Duration::from_secs(15);
    while tree.watch_count() != 1 && Instant::now() < deadline {
        tree.wait(Duration::from_millis(50)).unwrap();
    }
    assert_eq!(tree.watch_count(), 1);
}

#[test]
fn touch_m_and_a_permission_change_are_reported() {
    let scratch = Scratch::new("touch");
    let source = scratch.source();
    fs::write(source.join("photo.jpg"), b"x").unwrap();
    fs::write(source.join("script.sh"), b"x").unwrap();
    let mut tree = TreeWatch::arm(
        &source,
        options(&source, &[], controller_events(), LimitPolicy::Unchecked),
    )
    .unwrap();
    // `touch -m` through a descriptor opened only for reading: nothing is written or closed for
    // writing, and the kernel reports an mtime-only change as a modification, not an attribute
    // change. That is why the controller asks for `modify` as well as `attrib`.
    let file = fs::File::open(source.join("photo.jpg")).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    drop(file);
    let changes = gather(&mut tree, |seen| has(seen, "photo.jpg", "modify"));
    assert!(
        has(&changes, "photo.jpg", "modify"),
        "{:?}",
        entries(&changes)
    );
    assert!(
        !has(&changes, "photo.jpg", "close_write"),
        "{:?}",
        entries(&changes)
    );
    // A mode change is an attribute change.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(source.join("script.sh"), fs::Permissions::from_mode(0o700)).unwrap();
    let changes = gather(&mut tree, |seen| has(seen, "script.sh", "attrib"));
    assert!(
        has(&changes, "script.sh", "attrib"),
        "{:?}",
        entries(&changes)
    );
}

#[test]
fn dsm_managed_and_profile_excluded_paths_raise_nothing_and_cost_no_watch() {
    let scratch = Scratch::new("exclude");
    let source = scratch.source();
    fs::create_dir_all(source.join("photos/@eaDir/thumbs")).unwrap();
    fs::create_dir_all(source.join("#recycle/old")).unwrap();
    fs::create_dir_all(source.join("cache/deep")).unwrap();
    // DSM leaves @eaDir trees unreadable to other users: the walk must never try to enter one.
    fs::create_dir(source.join("locked")).unwrap();
    fs::create_dir(source.join("locked/@eaDir")).unwrap();
    std::process::Command::new("chmod")
        .args(["000"])
        .arg(source.join("locked/@eaDir"))
        .status()
        .unwrap();
    let mut tree = TreeWatch::arm(
        &source,
        options(
            &source,
            &["cache/", "*.tmp"],
            controller_events(),
            LimitPolicy::Unchecked,
        ),
    )
    .unwrap();
    // source, photos, locked.
    assert_eq!(tree.watch_count(), 3);
    fs::create_dir(source.join("photos/@eaDir/new")).unwrap();
    fs::write(source.join("photos/@eaDir/thumbs/t.jpg"), b"x").unwrap();
    fs::create_dir(source.join("@eaDir")).unwrap();
    fs::write(source.join("@eaDir/index"), b"x").unwrap();
    fs::write(source.join("#recycle/old/file"), b"x").unwrap();
    fs::write(source.join("cache/deep/blob"), b"x").unwrap();
    fs::write(source.join("photos/upload.tmp"), b"x").unwrap();
    fs::write(source.join("photos/desktop.ini"), b"x").unwrap();
    let changes = quiet(&mut tree, Duration::from_millis(400));
    assert!(changes.is_empty(), "{:?}", entries(&changes));
    assert_eq!(tree.watch_count(), 3);
    // An included file beside them still is news.
    fs::write(source.join("photos/kept.jpg"), b"x").unwrap();
    let changes = gather(&mut tree, |seen| {
        has(seen, "photos/kept.jpg", "close_write")
    });
    assert!(has(&changes, "photos/kept.jpg", "close_write"));
}

#[test]
fn an_unreadable_included_directory_fails_the_watch_by_name() {
    // Root reads anything; the refusal only exists for the package user, which is what DSM runs.
    let root = fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .any(|line| line.starts_with("Uid:") && line.split_whitespace().nth(1) == Some("0"));
    if root {
        eprintln!("skipped: root can watch a mode-0000 directory");
        return;
    }
    let scratch = Scratch::new("eacces");
    let source = scratch.source();
    fs::create_dir_all(source.join("private/inner")).unwrap();
    std::process::Command::new("chmod")
        .args(["000"])
        .arg(source.join("private"))
        .status()
        .unwrap();
    let refusal = TreeWatch::arm(
        &source,
        options(&source, &[], EventSet::all(), LimitPolicy::Unchecked),
    )
    .err()
    .unwrap();
    let line = refusal.to_string();
    assert_eq!(refusal.reason(), "watch_setup_failed", "{line}");
    assert!(
        line.starts_with("watch_setup_failed: cannot watch the directory: Permission denied"),
        "{line}"
    );
    assert!(line.ends_with("/source/private"), "{line}");
}

#[test]
fn an_edited_ignore_file_changes_what_is_watched() {
    let scratch = Scratch::new("rules");
    let source = scratch.source();
    fs::create_dir_all(source.join("build/out")).unwrap();
    fs::write(source.join(".sdsyncignore"), b"build/\n").unwrap();
    let mut tree = arm(&source);
    assert_eq!(tree.watch_count(), 1);
    fs::write(source.join(".sdsyncignore"), b"").unwrap();
    let changes = gather(&mut tree, |seen| has(seen, ".sdsyncignore", "close_write"));
    assert!(
        has(&changes, ".sdsyncignore", "close_write"),
        "{:?}",
        entries(&changes)
    );
    assert!(tree.is_watched("build/out"));
    assert_eq!(tree.watch_count(), 3);
    fs::write(source.join(".sdsyncignore"), b"build/\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while tree.watch_count() != 1 && Instant::now() < deadline {
        tree.wait(Duration::from_millis(50)).unwrap();
    }
    assert_eq!(tree.watch_count(), 1);
}

/// What a routine that may not delete arms: no deletions and no moves out reported. A removal
/// raises nothing, a move inside the tree still reports where the entry went, and the ignore
/// file's removal is still seen.
#[test]
fn an_event_set_without_removals_still_sees_moves_in_and_the_ignore_file_go() {
    let scratch = Scratch::new("no-removals");
    let source = scratch.source();
    fs::create_dir_all(source.join("docs/old")).unwrap();
    fs::create_dir_all(source.join("build")).unwrap();
    fs::write(source.join("docs/a.txt"), b"a").unwrap();
    fs::write(source.join("docs/old/b.txt"), b"b").unwrap();
    fs::write(source.join(".sdsyncignore"), b"build/\n").unwrap();
    let events = EventSet {
        delete: false,
        moved_from: false,
        modify: false,
        ..EventSet::all()
    };
    let mut tree = TreeWatch::arm(
        &source,
        options(&source, &[], events, LimitPolicy::Unchecked),
    )
    .unwrap();
    assert!(!tree.is_watched("build"));

    fs::remove_file(source.join("docs/a.txt")).unwrap();
    fs::rename(
        source.join("docs/old/b.txt"),
        scratch.outside().join("b.txt"),
    )
    .unwrap();
    let removed = quiet(&mut tree, Duration::from_millis(500));
    assert!(removed.is_empty(), "{:?}", entries(&removed));

    fs::rename(source.join("docs/old"), source.join("docs/new")).unwrap();
    let moved = gather(&mut tree, |seen| has(seen, "docs/new", "moved_to"));
    assert!(has(&moved, "docs/new", "moved_to"), "{:?}", entries(&moved));
    assert!(moved.iter().all(|change| !change.only_removes()));
    assert!(tree.is_watched("docs/new"));

    fs::remove_file(source.join(".sdsyncignore")).unwrap();
    let rules = gather(&mut tree, |seen| has(seen, ".sdsyncignore", "delete"));
    assert!(
        has(&rules, ".sdsyncignore", "delete"),
        "{:?}",
        entries(&rules)
    );
    assert!(rules.iter().all(|change| !change.only_removes()));
    assert!(tree.is_watched("build"), "the rules no longer exclude it");
}

#[test]
fn a_burst_the_kernel_cannot_queue_is_an_overflow_and_the_tree_recovers() {
    let scratch = Scratch::new("overflow");
    let source = scratch.source();
    let queued: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut tree = TreeWatch::arm(
        &source,
        options(&source, &[], controller_events(), LimitPolicy::Unchecked),
    )
    .unwrap();
    // Nothing reads while the burst is made: each file is a create and a close_write.
    let files = queued / 2 + 64;
    for index in 0..files {
        fs::write(source.join(format!("f{index}")), b"").unwrap();
    }
    fs::create_dir(source.join("made-during-overflow")).unwrap();
    let changes = gather(&mut tree, |seen| seen.contains(&Change::Overflow));
    assert!(
        changes.contains(&Change::Overflow),
        "no overflow after {files} files"
    );
    // The re-walk after the overflow picked up the directory whose event was lost.
    assert!(tree.is_watched("made-during-overflow"));
    fs::write(source.join("made-during-overflow/after"), b"x").unwrap();
    let changes = gather(&mut tree, |seen| {
        has(seen, "made-during-overflow/after", "close_write")
    });
    assert!(has(&changes, "made-during-overflow/after", "close_write"));
}

#[test]
fn a_deleted_source_root_ends_the_watch_by_name() {
    let scratch = Scratch::new("root-gone");
    let source = scratch.source();
    let mut tree = arm(&source);
    fs::remove_dir(&source).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let failure = loop {
        match tree.wait(Duration::from_millis(100)) {
            Ok(_) if Instant::now() < deadline => continue,
            Ok(_) => panic!("the root's removal was never reported"),
            Err(failure) => break failure,
        }
    };
    assert!(
        matches!(failure, WatchError::SourceGone { .. }),
        "{failure}"
    );
    assert_eq!(failure.reason(), "source_missing");
}

#[test]
fn a_missing_or_symlinked_source_is_refused_as_source_missing() {
    let scratch = Scratch::new("missing");
    let absent = scratch.0.join("absent");
    assert!(matches!(
        Filter::build(&absent, Vec::new()),
        Err(WatchError::SourceMissing { .. })
    ));
    let link = scratch.0.join("link");
    std::os::unix::fs::symlink(scratch.source(), &link).unwrap();
    let refusal = Filter::build(&link, Vec::new()).err().unwrap();
    assert_eq!(refusal.reason(), "source_missing", "{refusal}");
}

#[test]
fn a_tree_over_the_budget_is_refused_with_the_numbers_before_it_is_armed() {
    let scratch = Scratch::new("budget");
    let source = scratch.source();
    for index in 0..20 {
        fs::create_dir(source.join(format!("d{index}"))).unwrap();
    }
    // 80% of 20 is 16, less 2 held elsewhere: 14 directories fit, the 15th refuses.
    let refusal = TreeWatch::arm(
        &source,
        options(
            &source,
            &[],
            EventSet::all(),
            LimitPolicy::Fixed { limit: 20, held: 2 },
        ),
    )
    .err()
    .unwrap();
    assert_eq!(refusal.reason(), "watch_limit");
    let line = refusal.to_string();
    assert!(
        line.starts_with("watch_limit dirs=15 budget=14 limit=20 held=2: "),
        "{line}"
    );
    // The same tree within budget arms, and growing past it at runtime is refused too.
    let mut tree = TreeWatch::arm(
        &source,
        options(
            &source,
            &[],
            EventSet::all(),
            LimitPolicy::Fixed { limit: 30, held: 0 },
        ),
    )
    .unwrap();
    assert_eq!(tree.limits(), (Some(30), 0, Some(24)));
    for index in 0..4 {
        fs::create_dir(source.join(format!("e{index}"))).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let failure = loop {
        match tree.wait(Duration::from_millis(100)) {
            Ok(_) if Instant::now() < deadline => continue,
            Ok(_) => panic!("growth past the budget was never refused"),
            Err(failure) => break failure,
        }
    };
    assert_eq!(failure.reason(), "watch_limit", "{failure}");
}

/// The limit the kernel applies to this process: the per-user one, or the user namespace's own
/// where it is lower.
fn kernel_limit() -> u64 {
    let read = |path: &str| -> Option<u64> { fs::read_to_string(path).ok()?.trim().parse().ok() };
    let limit = read("/proc/sys/fs/inotify/max_user_watches").unwrap();
    read("/proc/sys/user/max_inotify_watches").map_or(limit, |namespace| limit.min(namespace))
}

#[test]
fn the_system_limit_policy_reads_the_kernel_limit() {
    let scratch = Scratch::new("system");
    let source = scratch.source();
    let tree = TreeWatch::arm(
        &source,
        options(&source, &[], EventSet::all(), LimitPolicy::System),
    )
    .unwrap();
    let limit = kernel_limit();
    let (read, held, budget) = tree.limits();
    assert_eq!(read, Some(limit));
    assert_eq!(budget, Some((limit / 10 * 8).saturating_sub(held)));
}

fn lowered_limit() -> Option<u64> {
    if std::env::var_os("SDSYNC_TEST_LOWERED_WATCH_LIMIT").is_none() {
        eprintln!("skipped: needs SDSYNC_TEST_LOWERED_WATCH_LIMIT and a lowered sysctl");
        return None;
    }
    let limit = kernel_limit();
    assert!(
        limit <= 4096,
        "SDSYNC_TEST_LOWERED_WATCH_LIMIT is set but max_user_watches is {limit}"
    );
    Some(limit)
}

#[test]
fn the_kernels_enospc_mid_walk_is_a_named_watch_limit_at_the_directory() {
    let Some(limit) = lowered_limit() else { return };
    let scratch = Scratch::new("enospc");
    let source = scratch.source();
    for index in 0..limit + 16 {
        fs::create_dir(source.join(format!("d{index}"))).unwrap();
    }
    // Unchecked, so it is the kernel and not the budget that refuses.
    let refusal = TreeWatch::arm(
        &source,
        options(&source, &[], EventSet::all(), LimitPolicy::Unchecked),
    )
    .err()
    .unwrap();
    let line = refusal.to_string();
    assert_eq!(refusal.reason(), "watch_limit", "{line}");
    assert!(line.contains(" at="), "{line}");
    assert!(line.contains("ENOSPC"), "{line}");
    // And with the budget checked, the refusal comes before the kernel's.
    let refusal = TreeWatch::arm(
        &source,
        options(&source, &[], EventSet::all(), LimitPolicy::System),
    )
    .err()
    .unwrap();
    let line = refusal.to_string();
    assert!(line.starts_with("watch_limit dirs="), "{line}");
    assert!(line.contains(&format!("limit={limit}")), "{line}");
}
