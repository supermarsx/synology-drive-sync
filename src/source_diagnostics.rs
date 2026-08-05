use std::path::{Path, PathBuf};

use crate::cancel::CancellationToken;
use crate::local::{self, EntryKind, IgnoreRules, LocalInventory};
use crate::{Error, Result};

/// Controls whether a source diagnostic also reads and hashes every payload file.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceDiagnosticOptions {
    pub hash_content: bool,
}

/// A successful, read-only diagnostic of one canonical local source tree.
///
/// `entries` counts files and directories below the root; the root itself is not an entry.
/// The retained inventory is ordered by relative path and contains each file's MD5 when
/// `hash_content` was requested. No partial report is returned when scanning or hashing fails.
#[derive(Debug)]
pub struct SourceDiagnosticReport {
    pub canonical_root: PathBuf,
    pub entries: usize,
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
    pub hashed_files: usize,
    pub inventory: LocalInventory,
}

/// Build the normal ignore rules and diagnose a source without contacting or modifying a NAS.
///
/// This deliberately delegates all path, link/reparse-point, portability, ignore-file, metadata,
/// and file-snapshot checks to the same scanner and hasher used by sync. Any error is propagated
/// and no partial report is returned.
pub fn diagnose_source(
    source: &Path,
    extra_patterns: &[String],
    options: SourceDiagnosticOptions,
    cancellation: &CancellationToken,
) -> Result<SourceDiagnosticReport> {
    cancellation.check()?;
    let rules = IgnoreRules::build(source, extra_patterns)?;
    diagnose_source_with_rules(source, &rules, options, cancellation)
}

fn diagnose_source_with_rules(
    source: &Path,
    rules: &IgnoreRules,
    options: SourceDiagnosticOptions,
    cancellation: &CancellationToken,
) -> Result<SourceDiagnosticReport> {
    cancellation.check()?;
    let inventory = local::scan(source, rules)?;
    cancellation.check()?;
    finish_diagnostic(inventory, options, cancellation)
}

fn finish_diagnostic(
    mut inventory: LocalInventory,
    options: SourceDiagnosticOptions,
    cancellation: &CancellationToken,
) -> Result<SourceDiagnosticReport> {
    cancellation.check()?;
    if options.hash_content {
        local::populate_content_md5(&mut inventory, cancellation)?;
    }
    cancellation.check()?;

    let mut files = 0_usize;
    let mut directories = 0_usize;
    let mut bytes = 0_u64;
    let mut hashed_files = 0_usize;
    for entry in inventory.entries.values() {
        match entry.kind {
            EntryKind::Directory => directories += 1,
            EntryKind::File => {
                files += 1;
                bytes = bytes.checked_add(entry.size).ok_or_else(|| {
                    Error::Message("local source byte count exceeds u64".to_owned())
                })?;
                if entry.content_md5.is_some() {
                    hashed_files += 1;
                }
            }
        }
    }
    if options.hash_content && hashed_files != files {
        return Err(Error::Message(
            "local source hashing completed without a digest for every file".to_owned(),
        ));
    }

    let entries = inventory.entries.len();
    let canonical_root = inventory.root.clone();
    Ok(SourceDiagnosticReport {
        canonical_root,
        entries,
        files,
        directories,
        bytes,
        hashed_files,
        inventory,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::local::{DEFAULT_IGNORE_FILE, LocalEntry};

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "sdsync-source-diagnostic-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reports_canonical_deterministic_counts_without_hashing() {
        let root = TestDir::new("counts");
        fs::create_dir(root.path().join("z-empty")).unwrap();
        fs::create_dir(root.path().join("folder")).unwrap();
        fs::write(root.path().join("folder/b.bin"), b"1234").unwrap();
        fs::write(root.path().join("a.txt"), b"ab").unwrap();

        let report = diagnose_source(
            root.path(),
            &[],
            SourceDiagnosticOptions::default(),
            &CancellationToken::default(),
        )
        .unwrap();

        assert_eq!(
            report.canonical_root,
            fs::canonicalize(root.path()).unwrap()
        );
        assert_eq!(report.inventory.root, report.canonical_root);
        assert_eq!(report.entries, 4);
        assert_eq!(report.files, 2);
        assert_eq!(report.directories, 2);
        assert_eq!(report.bytes, 6);
        assert_eq!(report.hashed_files, 0);
        assert_eq!(
            report
                .inventory
                .entries
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["a.txt", "folder", "folder/b.bin", "z-empty"]
        );
        assert!(
            report
                .inventory
                .entries
                .values()
                .all(|entry| entry.content_md5.is_none())
        );
    }

    #[test]
    fn full_hashing_records_every_digest_and_exact_byte_count() {
        let root = TestDir::new("hashes");
        fs::write(root.path().join("abc.bin"), b"abc").unwrap();
        fs::write(root.path().join("empty.bin"), b"").unwrap();

        let report = diagnose_source(
            root.path(),
            &[],
            SourceDiagnosticOptions { hash_content: true },
            &CancellationToken::default(),
        )
        .unwrap();

        assert_eq!(report.entries, 2);
        assert_eq!(report.files, 2);
        assert_eq!(report.directories, 0);
        assert_eq!(report.bytes, 3);
        assert_eq!(report.hashed_files, 2);
        assert_eq!(
            report.inventory.entries["abc.bin"]
                .content_md5
                .unwrap()
                .to_string(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            report.inventory.entries["empty.bin"]
                .content_md5
                .unwrap()
                .to_string(),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
    }

    #[test]
    fn default_ignore_file_and_extra_patterns_use_sync_semantics() {
        let root = TestDir::new("ignores");
        fs::write(root.path().join(DEFAULT_IGNORE_FILE), b"*.tmp\n").unwrap();
        fs::write(root.path().join("ignored.tmp"), b"tmp").unwrap();
        fs::write(root.path().join("also.skip"), b"skip").unwrap();
        fs::write(root.path().join("keep.txt"), b"keep").unwrap();

        let report = diagnose_source(
            root.path(),
            &["*.skip".to_owned()],
            SourceDiagnosticOptions { hash_content: true },
            &CancellationToken::default(),
        )
        .unwrap();

        assert_eq!(report.entries, 1);
        assert_eq!(report.files, 1);
        assert_eq!(report.bytes, 4);
        assert_eq!(report.hashed_files, 1);
        assert_eq!(
            report
                .inventory
                .entries
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["keep.txt"]
        );
    }

    #[test]
    fn cancellation_returns_no_partial_report() {
        let root = TestDir::new("cancelled");
        fs::write(root.path().join("payload.bin"), b"payload").unwrap();
        let cancellation = CancellationToken::default();
        cancellation.cancel();

        assert!(matches!(
            diagnose_source(
                root.path(),
                &[],
                SourceDiagnosticOptions { hash_content: true },
                &cancellation,
            ),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn scanner_safeguards_are_propagated_unchanged() {
        let root = TestDir::new("scanner-guard");
        fs::create_dir(root.path().join("@eaDir")).unwrap();

        assert!(matches!(
            diagnose_source(
                root.path(),
                &[],
                SourceDiagnosticOptions::default(),
                &CancellationToken::default(),
            ),
            Err(Error::UnsupportedLocalEntry { .. })
        ));
    }

    #[test]
    fn hash_snapshot_change_is_propagated_and_never_returns_counts() {
        let root = TestDir::new("changed-after-scan");
        let payload = root.path().join("payload.bin");
        fs::write(&payload, b"before").unwrap();
        let rules = IgnoreRules::build(root.path(), &[]).unwrap();
        let inventory = local::scan(root.path(), &rules).unwrap();
        fs::write(&payload, b"after-with-a-different-size").unwrap();

        assert!(matches!(
            finish_diagnostic(
                inventory,
                SourceDiagnosticOptions { hash_content: true },
                &CancellationToken::default(),
            ),
            Err(Error::SourceChanged(_))
        ));
    }

    #[test]
    fn byte_count_overflow_fails_closed() {
        let root = TestDir::new("overflow");
        let entries = BTreeMap::from([
            (
                "a".to_owned(),
                LocalEntry {
                    relative: "a".to_owned(),
                    full_path: root.path().join("a"),
                    kind: EntryKind::File,
                    size: u64::MAX,
                    mtime_ms: 0,
                    content_md5: None,
                },
            ),
            (
                "b".to_owned(),
                LocalEntry {
                    relative: "b".to_owned(),
                    full_path: root.path().join("b"),
                    kind: EntryKind::File,
                    size: 1,
                    mtime_ms: 0,
                    content_md5: None,
                },
            ),
        ]);
        let inventory = LocalInventory {
            root: fs::canonicalize(root.path()).unwrap(),
            entries,
        };

        assert!(matches!(
            finish_diagnostic(
                inventory,
                SourceDiagnosticOptions::default(),
                &CancellationToken::default(),
            ),
            Err(Error::Message(message)) if message.contains("exceeds u64")
        ));
    }
}
