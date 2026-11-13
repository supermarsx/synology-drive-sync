//! The converged-directories manifest: a realtime dispatch that cannot change
//! anything is skipped, never silently.
//!
//! A realtime routine runs after every quiet period that follows a change. Many
//! of those changes leave the directory exactly as the last successful run saw
//! it (a temporary file created and removed, a file renamed away and back, an
//! attribute written with the value it had), and a run costs seconds of NAS
//! traffic to find that out. So the daemon remembers, per directory, a digest of
//! the direct children as they were when a run that converged began, and at the
//! next dispatch reads the dirty directories again: if every one of them matches,
//! nothing a run could do has changed since the last convergence.
//!
//! What a digest holds, per child that the sync's own filter does not exclude:
//! the name and kind, and for a file (or a symlink) the size, mtime, ctime and
//! inode. A subdirectory contributes its name and inode only; its own children
//! are the digest of that directory. The ctime is what makes this sound: a
//! write, a rename-over or a chmod all set it to the time they happen and nothing
//! in user space can set it back, so a digest taken before such a change can
//! never equal one taken after it, provided the digest was not itself taken
//! inside the timestamp's granularity ([`RACY_MARGIN`]). A directory with a child
//! that changed that recently is read as racy and is never recorded.
//!
//! The manifest is memory only. It is empty after a restart (the first dispatch
//! of each directory runs), and it is emptied by anything that makes the last
//! convergence unprovable: a run that did not converge, a whole-tree mark, an
//! overflow, a change of what the routine watches. It is the daemon's own record
//! of what it dispatched, never the core's, so a core that is older or newer
//! changes nothing about it.
//!
//! What is not covered, and bounded by the daily whole-tree pass: a change made
//! to the NAS copy by somebody else (a dispatch never looked at the remote), and
//! a write that moves no timestamp (a page dirtied through a mapping and flushed
//! later), which the pair index's racy window describes.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use synology_drive_sync::watch::{Filter, profile_excludes};

/// The most directories one check reads. A dirty set larger than this is
/// dispatched as before: the check exists to save a run, not to become one.
pub(crate) const MAX_DIRS: usize = 256;

/// How long one check may take before the dispatch goes ahead without it.
pub(crate) const CHECK_DEADLINE: Duration = Duration::from_secs(5);

/// A child whose ctime is this close to the moment its directory was read could
/// still be rewritten without moving that ctime (kernel timestamps tick, and a
/// network or FAT-family mount rounds them to two seconds). Such a directory is
/// not recorded.
#[cfg_attr(not(unix), allow(dead_code))]
const RACY_MARGIN_NS: i128 = 3_000_000_000;

pub(crate) type DirDigest = [u8; 16];

/// One directory as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    pub(crate) digest: DirDigest,
    /// A child's ctime was inside the racy margin: the digest is not trusted
    /// as a record.
    pub(crate) racy: bool,
}

/// Directories read before a dispatch, each present and fully read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) dirs: BTreeMap<String, Facts>,
}

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Every dirty directory matches the manifest: nothing to dispatch.
    Unchanged,
    /// At least one differs (or is not in the manifest): dispatch, and keep the
    /// snapshot to record if the run converges.
    Changed(Snapshot),
    /// The check could not be made; the cause names the directory or the rule.
    /// The dispatch goes ahead exactly as it did before this module existed.
    Unavailable(String),
}

/// What a run records once it converged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    dirs: BTreeMap<String, DirDigest>,
}

impl Manifest {
    pub(crate) fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.dirs.len()
    }

    pub(crate) fn clear(&mut self) {
        self.dirs.clear();
    }

    /// A run whose scope held these directories converged: their digests, read
    /// before it began, are what the tree was when it did. A racy directory is
    /// forgotten rather than recorded.
    pub(crate) fn record(&mut self, snapshot: &Snapshot) {
        for (dir, facts) in &snapshot.dirs {
            if facts.racy {
                self.dirs.remove(dir);
            } else {
                self.dirs.insert(dir.clone(), facts.digest);
            }
        }
        // A manifest is bounded like a dirty set: past it, it is not trusted.
        if self.dirs.len() > super::changes::DIRTY_CAP {
            self.dirs.clear();
        }
    }

    fn matches(&self, dir: &str, facts: &Facts) -> bool {
        self.dirs.get(dir).is_some_and(|d| *d == facts.digest)
    }
}

/// What a check needs, owned so it can run on its own thread.
#[derive(Clone, Debug)]
pub(crate) struct CheckJob {
    pub(crate) source: std::path::PathBuf,
    pub(crate) fragment: std::path::PathBuf,
    pub(crate) dirs: BTreeSet<String>,
    pub(crate) manifest: Manifest,
}

/// Read the dirty directories and compare. Runs on a helper thread; `cancel`
/// ends it early (the dispatch gave up waiting).
pub(crate) fn check(job: &CheckJob, cancel: &AtomicBool) -> Outcome {
    if job.dirs.is_empty() || job.dirs.len() > MAX_DIRS {
        return Outcome::Unavailable(format!(
            "{} dirty directories, the check reads 1..={MAX_DIRS}",
            job.dirs.len()
        ));
    }
    let excludes = match profile_excludes(&job.fragment) {
        Ok(excludes) => excludes,
        Err(error) => return Outcome::Unavailable(format!("the profile's excludes: {error}")),
    };
    let filter = match Filter::build(&job.source, excludes) {
        Ok(filter) => filter,
        Err(error) => return Outcome::Unavailable(format!("the watch filter: {error}")),
    };
    let mut snapshot = Snapshot::default();
    let mut unchanged = true;
    for dir in &job.dirs {
        if cancel.load(Ordering::Relaxed) {
            return Outcome::Unavailable("the check was given up".to_owned());
        }
        match read_dir_facts(&job.source, &filter, dir) {
            Ok(Some(facts)) => {
                unchanged &= job.manifest.matches(dir, &facts);
                snapshot.dirs.insert(dir.clone(), facts);
            }
            // Gone: the run has to see that.
            Ok(None) => unchanged = false,
            Err(cause) => return Outcome::Unavailable(cause),
        }
    }
    if unchanged {
        Outcome::Unchanged
    } else {
        Outcome::Changed(snapshot)
    }
}

/// A relative directory made only of normal components.
#[cfg_attr(not(unix), allow(dead_code))]
fn relative_is_plain(relative: &str) -> bool {
    relative
        .split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..")
        || relative.is_empty()
}

#[cfg(not(unix))]
fn read_dir_facts(_: &Path, _: &Filter, relative: &str) -> Result<Option<Facts>, String> {
    Err(format!(
        "{relative}: change times are not read on this platform, so no directory can be proven unchanged"
    ))
}

/// `Ok(None)` when the directory is gone, `Err` naming the path when it (or an
/// entry in it) cannot be read as a whole.
#[cfg(unix)]
fn read_dir_facts(root: &Path, filter: &Filter, relative: &str) -> Result<Option<Facts>, String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    if !relative_is_plain(relative) {
        return Err(format!("{relative}: not a plain relative directory"));
    }
    let path = if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let now_ns = {
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| format!("the clock is before 1970: {error}"))?;
        i128::try_from(after.as_nanos()).unwrap_or(i128::MAX)
    };
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "{}: not a directory (or a symlink)",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    }
    let listing = match std::fs::read_dir(&path) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let mut children: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut racy = false;
    for item in listing {
        let entry = item.map_err(|error| format!("{}: {error}", path.display()))?;
        let name = entry.file_name();
        let child_path = entry.path();
        let meta = std::fs::symlink_metadata(&child_path)
            .map_err(|error| format!("{}: {error}", child_path.display()))?;
        let child = match name.to_str() {
            Some(text) if relative.is_empty() => text.to_owned(),
            Some(text) => format!("{relative}/{text}"),
            // A name that is not UTF-8 is one the sync cannot carry either;
            // its being there or not is a fact of the directory all the same.
            None => format!("{relative}/{}", String::from_utf8_lossy(name.as_bytes())),
        };
        if filter.excludes(&child, meta.is_dir()) {
            continue;
        }
        let mut fields = Vec::with_capacity(64);
        if meta.is_dir() {
            fields.push(b'd');
            fields.extend_from_slice(&meta.ino().to_le_bytes());
        } else {
            fields.push(if meta.is_file() { b'f' } else { b'o' });
            fields.extend_from_slice(&meta.len().to_le_bytes());
            fields.extend_from_slice(&meta.mtime().to_le_bytes());
            fields.extend_from_slice(&meta.mtime_nsec().to_le_bytes());
            fields.extend_from_slice(&meta.ctime().to_le_bytes());
            fields.extend_from_slice(&meta.ctime_nsec().to_le_bytes());
            fields.extend_from_slice(&meta.ino().to_le_bytes());
            let ctime_ns = i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec());
            if now_ns - ctime_ns < RACY_MARGIN_NS {
                racy = true;
            }
        }
        children.push((name.as_bytes().to_vec(), fields));
    }
    children.sort();
    let mut hash = Sha256::new();
    hash.update(b"sdsync.converged-dir.v1\0");
    for (name, fields) in &children {
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        hash.update(fields);
    }
    let full = hash.finalize();
    let mut digest = [0_u8; 16];
    digest.copy_from_slice(&full[..16]);
    Ok(Some(Facts { digest, racy }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "sdsync-converged-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("source")).unwrap();
            fs::write(
                root.join("fragment.toml"),
                "[profiles.p]\nexcludes = [\"*.skip\"]\n",
            )
            .unwrap();
            Self { root }
        }
        fn source(&self) -> PathBuf {
            self.root.join("source")
        }
        fn write(&self, relative: &str, content: &str) {
            let path = self.source().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        fn job(&self, dirs: &[&str], manifest: Manifest) -> CheckJob {
            CheckJob {
                source: self.source(),
                fragment: self.root.join("fragment.toml"),
                dirs: dirs.iter().map(|d| (*d).to_owned()).collect(),
                manifest,
            }
        }
        /// Age every file's ctime past the racy margin the only way a test can:
        /// by reading the facts a moment later than the margin allows, which
        /// would be slow, so the tests inspect `racy` instead and record by hand.
        fn facts(&self, dir: &str) -> Facts {
            let filter = Filter::build(&self.source(), vec!["*.skip".to_owned()]).unwrap();
            read_dir_facts(&self.source(), &filter, dir)
                .unwrap()
                .unwrap()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn recorded(dirs: &[(&str, Facts)]) -> Manifest {
        let mut manifest = Manifest::default();
        let mut snapshot = Snapshot::default();
        for (dir, facts) in dirs {
            snapshot.dirs.insert(
                (*dir).to_owned(),
                Facts {
                    digest: facts.digest,
                    racy: false,
                },
            );
        }
        manifest.record(&snapshot);
        manifest
    }

    #[test]
    fn a_directory_that_matches_its_record_is_unchanged() {
        let tree = Tree::new("same");
        tree.write("a/one.txt", "1");
        let manifest = recorded(&[("a", tree.facts("a"))]);
        let outcome = check(&tree.job(&["a"], manifest), &AtomicBool::new(false));
        assert_eq!(outcome, Outcome::Unchanged);
    }

    #[test]
    fn a_directory_that_was_never_recorded_is_changed_and_snapshotted() {
        let tree = Tree::new("new");
        tree.write("a/one.txt", "1");
        match check(
            &tree.job(&["a"], Manifest::default()),
            &AtomicBool::new(false),
        ) {
            Outcome::Changed(snapshot) => {
                assert!(snapshot.dirs.contains_key("a"));
                // Just written: its ctime is inside the margin.
                assert!(snapshot.dirs["a"].racy);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_file_added_removed_resized_or_replaced_changes_the_digest() {
        let tree = Tree::new("edit");
        tree.write("a/one.txt", "1");
        let before = tree.facts("a").digest;
        tree.write("a/two.txt", "2");
        let added = tree.facts("a").digest;
        assert_ne!(before, added);
        fs::remove_file(tree.source().join("a/two.txt")).unwrap();
        assert_eq!(
            tree.facts("a").digest,
            before,
            "an add and a remove is no change"
        );
        tree.write("a/one.txt", "12");
        assert_ne!(tree.facts("a").digest, before, "a different size");
        // Past the timestamp tick, so the rewrite below cannot share a ctime.
        std::thread::sleep(Duration::from_millis(50));
        tree.write("a/one.txt", "1");
        // Same size again, but written again: the ctime moved.
        assert_ne!(
            tree.facts("a").digest,
            before,
            "the rewrite moved the ctime"
        );
    }

    #[test]
    fn an_excluded_name_comes_and_goes_without_a_change() {
        let tree = Tree::new("excluded");
        tree.write("a/one.txt", "1");
        let before = tree.facts("a").digest;
        tree.write("a/scratch.skip", "x");
        assert_eq!(tree.facts("a").digest, before);
        let manifest = recorded(&[("a", tree.facts("a"))]);
        fs::remove_file(tree.source().join("a/scratch.skip")).unwrap();
        assert_eq!(
            check(&tree.job(&["a"], manifest), &AtomicBool::new(false)),
            Outcome::Unchanged
        );
    }

    #[test]
    fn one_changed_directory_among_matching_ones_changes_the_set() {
        let tree = Tree::new("one-of-two");
        tree.write("a/one.txt", "1");
        tree.write("b/two.txt", "2");
        let manifest = recorded(&[("a", tree.facts("a")), ("b", tree.facts("b"))]);
        tree.write("b/three.txt", "3");
        assert!(matches!(
            check(&tree.job(&["a", "b"], manifest), &AtomicBool::new(false)),
            Outcome::Changed(_)
        ));
    }

    #[test]
    fn a_missing_directory_is_changed_and_a_symlinked_one_is_unavailable() {
        let tree = Tree::new("gone");
        tree.write("a/one.txt", "1");
        let manifest = recorded(&[("a", tree.facts("a"))]);
        fs::remove_dir_all(tree.source().join("a")).unwrap();
        assert!(matches!(
            check(&tree.job(&["a"], manifest.clone()), &AtomicBool::new(false)),
            Outcome::Changed(_)
        ));
        std::os::unix::fs::symlink("/", tree.source().join("a")).unwrap();
        match check(&tree.job(&["a"], manifest), &AtomicBool::new(false)) {
            Outcome::Unavailable(cause) => assert!(cause.contains("not a directory"), "{cause}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_racy_directory_is_never_recorded() {
        let tree = Tree::new("racy");
        tree.write("a/one.txt", "1");
        let facts = tree.facts("a");
        assert!(facts.racy);
        let mut manifest = Manifest::default();
        let mut snapshot = Snapshot::default();
        snapshot.dirs.insert("a".to_owned(), facts);
        manifest.record(&snapshot);
        assert!(manifest.is_empty());
    }

    #[test]
    fn a_path_that_is_not_plain_is_refused() {
        let tree = Tree::new("plain");
        let outcome = check(
            &tree.job(&["../x"], Manifest::default()),
            &AtomicBool::new(false),
        );
        assert!(matches!(outcome, Outcome::Unavailable(_)), "{outcome:?}");
        let outcome = check(&tree.job(&[], Manifest::default()), &AtomicBool::new(false));
        assert!(matches!(outcome, Outcome::Unavailable(_)), "{outcome:?}");
    }

    #[test]
    fn an_unreadable_fragment_is_unavailable_and_named() {
        let tree = Tree::new("fragment");
        tree.write("a/one.txt", "1");
        let mut job = tree.job(&["a"], Manifest::default());
        job.fragment = tree.root.join("absent.toml");
        match check(&job, &AtomicBool::new(false)) {
            Outcome::Unavailable(cause) => assert!(cause.contains("absent.toml"), "{cause}"),
            other => panic!("{other:?}"),
        }
    }
}
