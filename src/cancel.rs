use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::progress_record::ProgressRecorder;
use crate::{Error, Result};

/// The cancellation signal, and the progress sink that rides alongside it.
///
/// The sink lives here rather than in a parameter of its own because this token is *already*
/// threaded through every long-running walk in the crate -- the scan, the remote inventory, the
/// digest pass, and the execution loop all take one. A separate parameter would have to be added
/// to each of those signatures and to everything between them, for a value that is `None` on every
/// path except the one the DSM bridge drives. Carrying it here makes the phase and count call
/// sites free to add and costs a null-pointer test when no recorder is attached.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    progress: Option<Arc<ProgressRecorder>>,
}

impl CancellationToken {
    /// Attach a progress sink. Clones made after this share the same recorder.
    #[must_use]
    pub fn with_progress(mut self, recorder: Arc<ProgressRecorder>) -> Self {
        self.progress = Some(recorder);
        self
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Publish entry into a named phase of the running operation.
    ///
    /// Deliberately not fallible and deliberately silent about an id the operation's catalogue
    /// does not contain: progress is advisory, and a phase boundary must never be a place a sync
    /// can fail.
    pub fn phase(&self, id: &str) {
        if let Some(recorder) = &self.progress {
            recorder.phase(id);
        }
    }

    /// Count one item within the current phase.
    pub fn tick(&self) {
        if let Some(recorder) = &self.progress {
            recorder.tick();
        }
    }

    /// Count `amount` items within the current phase.
    pub fn add(&self, amount: u64) {
        if let Some(recorder) = &self.progress {
            recorder.add(amount);
        }
    }
}
