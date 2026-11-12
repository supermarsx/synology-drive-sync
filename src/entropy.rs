//! A bounded wait for the system random number generator.
//!
//! On a kernel without `getrandom(2)` (DSM 7 still ships 3.10), the `getrandom` crate that `ring`
//! draws from waits once per process in `poll(/dev/random, POLLIN, -1)` until the kernel's entropy
//! count reaches 64 bits. On a drained pool that wait has no end, and it sits inside the first TLS
//! client construction, which no request deadline covers: the run or the daemon's job just hangs
//! with no record. So the first draw is made on a helper thread, and its caller waits for it with
//! a bound. On expiry the failure is named, [`Error::EntropyUnavailable`], and carried by the
//! ordinary failure paths into the run's result record, log and state.
//!
//! This module does not change the random source: the helper makes the very draw TLS would make
//! (the rustls `ring` provider's), so a successful wait means TLS no longer waits. A helper that
//! is still blocked when the bound expires keeps waiting; a later caller waits on that same
//! helper again, so a daemon recovers once the pool fills, without a second blocked thread.

use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use crate::error::{Error, Result};

/// How long a caller waits for the first draw before naming `entropy_unavailable`.
pub const ENTROPY_WAIT: Duration = Duration::from_secs(15);

#[derive(Debug)]
enum State {
    Idle,
    Waiting,
    Ready,
    Failed(String),
}

/// The progress of one process's first draw from the system generator.
pub struct EntropyGate {
    state: Mutex<State>,
    changed: Condvar,
}

impl EntropyGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::Idle),
            changed: Condvar::new(),
        })
    }

    /// Start the draw on a helper thread if nobody has, without waiting for it.
    ///
    /// Returns an error only when the helper thread could not be started, which is named like a
    /// timeout: the generator was never asked.
    pub fn begin<F>(self: &Arc<Self>, fill: F) -> Result<()>
    where
        F: FnOnce() -> std::result::Result<(), String> + Send + 'static,
    {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !matches!(*state, State::Idle | State::Failed(_)) {
            return Ok(());
        }
        *state = State::Waiting;
        let gate = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("sdsync-entropy".to_owned())
            .spawn(move || {
                let outcome = fill();
                let mut state = gate.state.lock().unwrap_or_else(PoisonError::into_inner);
                *state = match outcome {
                    Ok(()) => State::Ready,
                    Err(cause) => State::Failed(cause),
                };
                gate.changed.notify_all();
            });
        match spawned {
            Ok(_) => Ok(()),
            Err(error) => {
                *state = State::Idle;
                Err(Error::EntropyUnavailable {
                    waited_secs: 0,
                    cause: format!("the helper thread could not be started: {error}"),
                })
            }
        }
    }

    /// Wait for the draw [`Self::begin`] started (starting it first if needed), for at most
    /// `timeout`.
    pub fn wait<F>(self: &Arc<Self>, fill: F, timeout: Duration) -> Result<()>
    where
        F: FnOnce() -> std::result::Result<(), String> + Send + 'static,
    {
        self.begin(fill)?;
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| matches!(state, State::Waiting))
            .unwrap_or_else(PoisonError::into_inner);
        match &*state {
            State::Ready => Ok(()),
            State::Waiting => Err(Error::EntropyUnavailable {
                waited_secs: timeout.as_secs(),
                cause: "the first draw is still waiting for the kernel's entropy pool".to_owned(),
            }),
            State::Failed(cause) => {
                let cause = cause.clone();
                // Not remembered: a later caller asks the generator again.
                *state = State::Idle;
                Err(Error::EntropyUnavailable {
                    waited_secs: 0,
                    cause,
                })
            }
            State::Idle => Err(Error::EntropyUnavailable {
                waited_secs: 0,
                cause: "the draw was never started".to_owned(),
            }),
        }
    }
}

/// The draw TLS would make first: the `ring` provider's, which is the one this build installs.
fn system_fill() -> std::result::Result<(), String> {
    let mut sample = [0_u8; 8];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut sample)
        .map_err(|_| "the system random number generator reported an error".to_owned())
}

fn process_gate() -> &'static Arc<EntropyGate> {
    static GATE: OnceLock<Arc<EntropyGate>> = OnceLock::new();
    GATE.get_or_init(EntropyGate::new)
}

/// Start this process's first draw in the background. A caller that will need TLS later calls
/// this early, so the pool's wait overlaps its other work; [`ensure_ready`] is the bounded wait.
pub fn begin_warmup() -> Result<()> {
    process_gate().begin(system_fill)
}

/// Wait, for at most [`ENTROPY_WAIT`], until the system generator answers, or fail with the named
/// [`Error::EntropyUnavailable`]. Free once the first draw is done.
pub fn ensure_ready() -> Result<()> {
    process_gate().wait(system_fill, ENTROPY_WAIT)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    /// A source that never becomes ready, the 3.10 kernel's drained pool: the wait is bounded, the
    /// failure is named with the bound, and nothing hangs.
    #[test]
    fn a_source_that_never_becomes_ready_fails_by_name_within_the_bound() {
        let gate = EntropyGate::new();
        let (release, blocked) = mpsc::channel::<()>();
        let started = Instant::now();
        let error = gate
            .wait(
                move || {
                    let _ = blocked.recv();
                    Ok(())
                },
                Duration::from_millis(100),
            )
            .expect_err("a never-ready source must not report ready");
        assert!(started.elapsed() < Duration::from_secs(5), "bounded");
        assert!(matches!(error, Error::EntropyUnavailable { .. }), "{error}");
        let text = error.to_string();
        assert!(text.starts_with("entropy_unavailable: "), "{text}");
        assert!(text.contains("kernel's entropy pool"), "{text}");
        // A later caller waits on the same helper (no second thread) and recovers once it is
        // released.
        release.send(()).expect("the helper is still waiting");
        gate.wait(|| Err("a second helper must not start".to_owned()), Duration::from_secs(5))
            .expect("ready after the pool filled");
    }

    #[test]
    fn a_ready_source_is_free_and_asked_once() {
        let gate = EntropyGate::new();
        gate.wait(|| Ok(()), Duration::from_secs(5)).expect("ready");
        // Asked once: the second closure would fail if it ran.
        gate.wait(|| Err("asked twice".to_owned()), Duration::from_millis(1))
            .expect("still ready, without asking again");
    }

    #[test]
    fn a_generator_error_is_named_and_asked_again_next_time() {
        let gate = EntropyGate::new();
        let error = gate
            .wait(|| Err("boom".to_owned()), Duration::from_secs(5))
            .expect_err("an error is named");
        assert!(error.to_string().contains("boom"), "{error}");
        gate.wait(|| Ok(()), Duration::from_secs(5))
            .expect("the next caller asks again");
    }

    /// The process's own generator answers in tests and on any kernel with `getrandom(2)`.
    #[test]
    fn the_system_generator_answers() {
        ensure_ready().expect("the system generator is ready");
        ensure_ready().expect("and stays ready");
    }
}
