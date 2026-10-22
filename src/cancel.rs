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
    /// The token this one was derived from. Cancelling the parent cancels this one; cancelling
    /// this one leaves the parent alone.
    parent: Option<Arc<CancellationToken>>,
    progress: Option<Arc<ProgressRecorder>>,
}

impl CancellationToken {
    /// Attach a progress sink. Clones made after this share the same recorder.
    #[must_use]
    pub fn with_progress(mut self, recorder: Arc<ProgressRecorder>) -> Self {
        self.progress = Some(recorder);
        self
    }

    /// A token that is cancelled with this one, and can also be cancelled on its own.
    ///
    /// For abandoning a bounded piece of work (one digest batch whose answer is already decided)
    /// without telling the rest of the run to stop. Progress is shared with this token.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            parent: Some(Arc::new(self.clone())),
            progress: self.progress.clone(),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.is_cancelled())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_is_cancelled_by_its_parent_but_not_the_reverse() {
        let parent = CancellationToken::default();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(
            !parent.is_cancelled(),
            "abandoning a batch must not stop the run"
        );

        let other = parent.child();
        parent.cancel();
        assert!(other.is_cancelled(), "stopping the run stops every batch");
        assert!(matches!(other.check(), Err(Error::Cancelled)));
    }
}
