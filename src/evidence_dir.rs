//! The directory discipline shared by the stores of evidence the engine keeps on disk: the
//! status cache and the verified-pair index.
//!
//! Each store removes files from its directory, so it must be sure the directory is its own. Both
//! follow one set of rules, kept here so the two cannot drift:
//!
//! - a marker file names the directory's owner; a directory carrying another owner's marker, or
//!   holding any name the store could not have written, is refused with the offenders named;
//! - a directory without a marker but holding only the store's own names is adopted (an older
//!   package wrote it before markers existed) by writing the marker;
//! - the directory must be a real directory that grants no write access to group or other;
//! - a profile name becomes a file name only if [`name_is_safe`] accepts it.

use std::fs;
use std::io::Write as _;
use std::path::Path;

/// One store's claim on a directory.
pub struct DirectorySpec<'a> {
    /// The marker's file name (`.sdsync-status-cache`).
    pub marker: &'a str,
    /// What every marker of this store starts with, whichever schema wrote it.
    pub marker_prefix: &'a str,
    /// The line written into a new marker (the store's schema).
    pub marker_line: &'a str,
    /// The store's name in a refusal (`status-cache`).
    pub kind: &'a str,
    /// The option that points the store at a directory (`--status-cache`).
    pub flag: &'a str,
    /// The store as a subject (`the cache`) and as an owner (`this cache`).
    pub subject: &'a str,
    pub this: &'a str,
    /// Whether the store could have written an entry of this name.
    pub owns: &'a dyn Fn(&std::ffi::OsStr) -> bool,
}

/// Decide whether `directory` may be used by the store `spec` describes, claiming it if it may.
///
/// `Ok(n)` with the number of the store's files an unmarked directory already held when it was
/// adopted, so the caller can name the adoption; 0 when nothing was adopted.
pub fn claim_directory(directory: &Path, spec: &DirectorySpec<'_>) -> Result<usize, String> {
    let marker = directory.join(spec.marker);
    let refuse = |why: String| {
        format!(
            "not a dedicated {} directory: {why}; point {} at a directory used for nothing else",
            spec.kind, spec.flag
        )
    };
    if marker_is_ours(&marker, spec, &refuse)? {
        return Ok(0);
    }

    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "{} could not be listed to check it is dedicated to {}: {error}",
            directory.display(),
            spec.subject
        )
    })?;
    let mut foreign = Vec::new();
    let mut count = 0_usize;
    let mut owned = 0_usize;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "{} could not be listed to check it is dedicated to {}: {error}",
                directory.display(),
                spec.subject
            )
        })?;
        if (spec.owns)(&entry.file_name()) {
            owned += 1;
        } else {
            count += 1;
            if foreign.len() < 3 {
                foreign.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    if count > 0 {
        let more = if count > foreign.len() {
            format!(" and {} more", count - foreign.len())
        } else {
            String::new()
        };
        return Err(refuse(format!(
            "it has no {} marker and holds {count} entr{} {} did not write ({}{more})",
            spec.marker,
            if count == 1 { "y" } else { "ies" },
            spec.subject,
            foreign.join(", ")
        )));
    }

    let written = (|| -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&marker)?;
        file.write_all(spec.marker_line.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()
    })();
    match written {
        Ok(()) => Ok(owned),
        // Another pass claimed the directory between the check and the write.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if marker_is_ours(&marker, spec, &refuse)? {
                Ok(0)
            } else {
                Err(refuse(format!(
                    "{} appeared with unexpected contents",
                    marker.display()
                )))
            }
        }
        Err(error) => Err(format!(
            "the {} marker could not be written in {}: {error}",
            spec.marker,
            directory.display()
        )),
    }
}

/// `Ok(true)` when `marker` is this store's marker, `Ok(false)` when there is none, and an error
/// (already phrased as a refusal) when something else is sitting at that name.
fn marker_is_ours(
    marker: &Path,
    spec: &DirectorySpec<'_>,
    refuse: &dyn Fn(String) -> String,
) -> Result<bool, String> {
    // Another pass may be part-way through writing the marker it just created; give it a moment
    // rather than call an empty file foreign.
    for attempt in 0..5 {
        let metadata = match fs::symlink_metadata(marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "{} could not be examined: {error}",
                    marker.display()
                ));
            }
        };
        if !metadata.is_file() {
            return Err(refuse(format!(
                "{} is not a regular file",
                marker.display()
            )));
        }
        let mut head = Vec::new();
        fs::File::open(marker)
            .and_then(|file| {
                std::io::Read::read_to_end(&mut std::io::Read::take(file, 256), &mut head)
            })
            .map_err(|error| format!("{} could not be read: {error}", marker.display()))?;
        if head.starts_with(spec.marker_prefix.as_bytes()) {
            return Ok(true);
        }
        if head.is_empty() && attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        return Err(refuse(format!(
            "{} was not written by {}",
            marker.display(),
            spec.this
        )));
    }
    Err(refuse(format!(
        "{} was not written by {}",
        marker.display(),
        spec.this
    )))
}

/// A directory is usable only if it is a real directory, not a symbolic link, and grants no write
/// access to group or other.
pub fn directory_is_private(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_dir() {
        return Err("not a directory (a link or a file)".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.mode() & 0o022 != 0 {
            return Err("writable by group or other".to_owned());
        }
    }
    Ok(())
}

/// The profile name becomes a filename, so it is constrained exactly as the package's own
/// `cache_name_is_safe` constrains a cache key: no traversal, no separators, nothing hidden.
pub fn name_is_safe(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name.starts_with('.') || name.contains("..") {
        return false;
    }
    name.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
