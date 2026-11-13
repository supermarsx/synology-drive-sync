pub mod file_station_mock;

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// `seed` repeated to exactly [`synology_drive_sync::plan::SERVER_COPY_MIN_BYTES`], the smallest
/// moved file a plan server-copies. A smaller fixture takes the verified upload instead and never
/// reaches the copy path it means to test.
pub fn copy_payload(seed: &[u8]) -> Vec<u8> {
    let size = usize::try_from(synology_drive_sync::plan::SERVER_COPY_MIN_BYTES)
        .expect("the copy threshold fits usize");
    let mut payload = seed.repeat(size / seed.len() + 1);
    payload.truncate(size);
    payload
}

pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        for _ in 0..128 {
            let sequence = TEST_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sdsync-e2e-{label}-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated E2E directory: {error}"),
            }
        }
        panic!("create isolated E2E directory: exhausted unique path attempts")
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn child(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    pub fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.child(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
