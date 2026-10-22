//! A real walk feeding a real plan: what the walk declines to carry must never become a deletion.
//!
//! The unit tests prove each half. This proves the seam between them through the public API, on a
//! real directory, because the failure it guards against is a mismatch between the two halves: a
//! walk that skips a path and a planner that reads the skip as absence would delete the remote copy
//! of every file the walk could not read.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use synology_drive_sync::api::{RemoteEntry, RemoteInventory};
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::local::{EntryKind, IgnoreRules, scan};
use synology_drive_sync::path::RemoteRoot;
use synology_drive_sync::plan::{CompareMode, PlanOptions, Scope, SyncPlan, build_plan};

fn temp_tree(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "sdsync-scan-skip-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn remote(entries: &[(&str, EntryKind)]) -> RemoteInventory {
    RemoteInventory {
        root_exists: true,
        entries: entries
            .iter()
            .map(|(relative, kind)| {
                (
                    (*relative).to_owned(),
                    RemoteEntry {
                        relative: (*relative).to_owned(),
                        remote_path: format!("/share/root/{relative}"),
                        kind: *kind,
                        size: 1,
                        mtime_seconds: 1,
                        mount_point_type: None,
                        content_md5: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    }
}

fn mirror(source: &Path, remote: &RemoteInventory) -> SyncPlan {
    let rules = IgnoreRules::build(source, &[]).unwrap();
    let local = scan(source, &rules, &CancellationToken::default()).unwrap();
    build_plan(
        &RemoteRoot::parse("/share/root").unwrap(),
        &local,
        remote,
        &rules,
        &PlanOptions {
            delete: true,
            allow_empty_source: false,
            max_delete: 100,
            compare: CompareMode::Metadata,
            server_copy: false,
            scope: Scope::root(),
        },
    )
    .unwrap()
}

fn deleted(plan: &SyncPlan) -> Vec<&str> {
    plan.pre_deletes
        .iter()
        .chain(&plan.post_deletes)
        .map(|action| action.relative.as_str())
        .collect()
}

#[cfg(windows)]
fn set_attributes(target: &Path, flags: &[&str]) -> bool {
    std::process::Command::new("attrib")
        .args(flags)
        .arg(target)
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn make_link(link: &Path, target: &Path) -> bool {
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(unix)]
fn make_link(link: &Path, target: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

/// The ordinary Windows profile folder: `desktop.ini` Hidden+System, one other SYSTEM file and a
/// junction. The run used to abort on the first of them; now it syncs the document, reports the
/// two skips, and deletes only the one genuinely remote-only file.
#[cfg(windows)]
#[test]
fn a_windows_profile_folder_syncs_and_its_skipped_entries_are_never_deleted() {
    let source = temp_tree("windows-profile");
    let outside = temp_tree("windows-profile-target");
    let documents = source.join("Documents");
    fs::create_dir(&documents).unwrap();
    fs::write(documents.join("report.docx"), b"report").unwrap();
    let ini = documents.join("desktop.ini");
    fs::write(&ini, b"[.ShellClassInfo]").unwrap();
    let system = documents.join("system.dat");
    fs::write(&system, b"system").unwrap();
    if !set_attributes(&ini, &["+H", "+S"])
        || !set_attributes(&system, &["+S"])
        || !make_link(&source.join("Linked"), &outside)
    {
        eprintln!("skipping: could not set attributes or create a junction");
        return;
    }

    let plan = mirror(
        &source,
        &remote(&[
            ("Documents", EntryKind::Directory),
            ("Documents/desktop.ini", EntryKind::File),
            ("Documents/system.dat", EntryKind::File),
            ("Documents/SYSTEM.DAT", EntryKind::File),
            ("Linked", EntryKind::Directory),
            ("Linked/inside.txt", EntryKind::File),
            ("stale.txt", EntryKind::File),
        ]),
    );

    assert_eq!(deleted(&plan), ["stale.txt"]);
    assert_eq!(
        plan.uploads
            .iter()
            .map(|upload| upload.local.relative.as_str())
            .collect::<Vec<_>>(),
        ["Documents/report.docx"]
    );
    assert_eq!(
        plan.skipped
            .iter()
            .map(|skip| (skip.relative.as_str(), skip.reason.as_str()))
            .collect::<Vec<_>>(),
        [
            ("Documents/system.dat", "windows-attributes"),
            ("Linked", "link-or-reparse-point"),
        ]
    );

    let _ = set_attributes(&ini, &["-H", "-S"]);
    let _ = set_attributes(&system, &["-S"]);
    let _ = fs::remove_dir(source.join("Linked"));
    fs::remove_dir_all(&source).unwrap();
    fs::remove_dir_all(&outside).unwrap();
}

/// The macOS shape: `.DS_Store` beside the payload and a symlink inside the tree.
#[cfg(unix)]
#[test]
fn os_metadata_and_links_neither_fail_the_scan_nor_become_deletions() {
    let source = temp_tree("unix-metadata");
    let outside = temp_tree("unix-metadata-target");
    fs::write(source.join(".DS_Store"), b"bud1").unwrap();
    fs::write(source.join("photo.jpg"), b"jpg").unwrap();
    if !make_link(&source.join("Linked"), &outside) {
        eprintln!("skipping: could not create a symlink");
        return;
    }

    let plan = mirror(
        &source,
        &remote(&[
            (".DS_Store", EntryKind::File),
            ("Linked", EntryKind::Directory),
            ("Linked/inside.txt", EntryKind::File),
            ("stale.txt", EntryKind::File),
        ]),
    );

    assert_eq!(deleted(&plan), ["stale.txt"]);
    assert_eq!(
        plan.skipped
            .iter()
            .map(|skip| (skip.relative.as_str(), skip.reason.as_str()))
            .collect::<Vec<_>>(),
        [("Linked", "link-or-reparse-point")]
    );

    fs::remove_file(source.join("Linked")).unwrap();
    fs::remove_dir_all(&source).unwrap();
    fs::remove_dir_all(&outside).unwrap();
}
