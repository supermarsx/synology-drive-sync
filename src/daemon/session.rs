//! The DSM session the daemon keeps between runs (fix9 P7, perf design §1).
//!
//! A run that logged in hands its session back on its standard input instead of logging out; the
//! daemon keeps it for an idle TTL and lends it to the next run of the same profile, which skips
//! discovery, login and logout. The channel is an anonymous socket pair, one end the child's
//! standard input, so the session has no name anywhere a reader could find it.
//!
//! Invariants:
//! - A session lives only in this process's memory, zeroized on drop ([`SessionHandover`]), and in
//!   the socket buffer between the two processes. It is never in a note (notes name the profile
//!   and what happened), a state record, argv or the environment.
//! - Every session the daemon held ends in a logout: TTL lapsed, replaced by a newer one, lent to
//!   a run that did not hand it back, or the service stopping. Except one DSM already refused:
//!   the run said so, and logging it out again would only be noise.
//! - The binding (which credentials a session belongs to) is the run's to check, not the
//!   daemon's: the run ignores a session bound to other credentials, names it, and hands back its
//!   own, and the daemon then logs the ignored one out.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use synology_drive_sync::api::SessionHandover;

use super::routines::Level;

/// How long an unused session is kept: under DSM's default 15-minute idle logout, so a lent
/// session is normally still alive.
pub(crate) const IDLE_TTL: Duration = Duration::from_secs(10 * 60);
/// The shortest the TTL is ever shortened to after a refused session.
const MIN_TTL: Duration = Duration::from_secs(60);
/// One logout request, from the daemon.
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the run's handback may still be in flight once the run has exited. The run writes it
/// and shuts its side down before it exits, so this is only reached by a run that never did.
#[cfg(unix)]
const HANDBACK_WAIT: Duration = Duration::from_secs(2);
/// Notes kept for the control thread between two turns; older ones are counted, not kept.
const MAX_NOTES: usize = 64;

/// One line for the controller log. Never carries the session.
#[derive(Debug)]
pub(crate) struct Note {
    pub(crate) level: Level,
    pub(crate) event: &'static str,
    pub(crate) detail: String,
}

/// Ends one session at DSM. Production: [`SessionHandover::log_out`].
pub(crate) type LogOut = Arc<dyn Fn(&SessionHandover) -> Result<(), String> + Send + Sync>;

struct Held {
    handover: SessionHandover,
    idle_since: Instant,
}

struct Shared {
    held: BTreeMap<String, Held>,
    ttl: Duration,
    notes: Vec<Note>,
    dropped_notes: usize,
}

impl Shared {
    fn note(&mut self, level: Level, event: &'static str, detail: String) {
        if self.notes.len() >= MAX_NOTES {
            self.dropped_notes = self.dropped_notes.saturating_add(1);
            return;
        }
        self.notes.push(Note {
            level,
            event,
            detail,
        });
    }
}

#[derive(Clone)]
struct Core {
    shared: Arc<Mutex<Shared>>,
    log_out: LogOut,
}

impl Core {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// End `handover` at DSM on a worker thread, and note how it went. Inline when no thread can
    /// be started: a slower turn, never a session left unended.
    fn retire(&self, profile: &str, handover: SessionHandover, why: &'static str) {
        // Shared, so a thread that cannot be started leaves the session here to end inline.
        let slot = Arc::new(Mutex::new(Some(handover)));
        let take = |slot: &Mutex<Option<SessionHandover>>| {
            slot.lock().unwrap_or_else(PoisonError::into_inner).take()
        };
        let worker = {
            let core = self.clone();
            let name = profile.to_owned();
            let slot = Arc::clone(&slot);
            move || {
                if let Some(handover) = take(&slot) {
                    core.log_out_now(&name, &handover, why);
                }
            }
        };
        let spawned = std::thread::Builder::new()
            .name("sdsyncd-logout".into())
            .spawn(worker);
        if let Err(error) = spawned {
            self.lock().note(
                Level::Warn,
                "session_logout_inline",
                format!("profile={profile} cause=no logout thread could be started: {error}"),
            );
            if let Some(handover) = take(&slot) {
                self.log_out_now(profile, &handover, why);
            }
        }
    }

    fn log_out_now(&self, profile: &str, handover: &SessionHandover, why: &'static str) {
        let outcome = (self.log_out)(handover);
        let mut shared = self.lock();
        match outcome {
            Ok(()) => shared.note(
                Level::Info,
                "session_logged_out",
                format!("profile={profile} why={why}"),
            ),
            Err(cause) => shared.note(
                Level::Warn,
                "session_logout_failed",
                format!(
                    "profile={profile} why={why} cause={cause}; DSM ends the session on its own idle timer"
                ),
            ),
        }
    }

    /// Keep `handover` as `profile`'s session; one already kept is replaced and ended.
    fn keep(&self, profile: &str, handover: SessionHandover) {
        let replaced = self.lock().held.insert(
            profile.to_owned(),
            Held {
                handover,
                idle_since: Instant::now(),
            },
        );
        if let Some(old) = replaced {
            self.retire(profile, old.handover, "replaced");
        }
    }
}

/// The daemon's sessions, one per profile. Disabled (`ttl: None`): nothing is lent and runs log
/// in and out themselves, as before.
pub(crate) struct Sessions {
    core: Option<Core>,
}

impl Sessions {
    pub(crate) fn new(ttl: Option<Duration>) -> Self {
        Self::with_log_out(
            ttl,
            Arc::new(|handover: &SessionHandover| {
                handover
                    .log_out(LOGOUT_TIMEOUT)
                    .map_err(|error| error.to_string())
            }),
        )
    }

    pub(crate) fn with_log_out(ttl: Option<Duration>, log_out: LogOut) -> Self {
        Self {
            core: ttl.map(|ttl| Core {
                shared: Arc::new(Mutex::new(Shared {
                    held: BTreeMap::new(),
                    ttl,
                    notes: Vec::new(),
                    dropped_notes: 0,
                })),
                log_out,
            }),
        }
    }

    /// The channel for one run of `profile`, lending its session when one is kept and still
    /// fresh. `None` when reuse is off.
    pub(crate) fn lend(&self, profile: &str) -> Option<Lease> {
        let core = self.core.as_ref()?;
        let now = Instant::now();
        let (lent, idle) = {
            let mut shared = core.lock();
            let ttl = shared.ttl;
            match shared.held.remove(profile) {
                Some(held) if now.saturating_duration_since(held.idle_since) < ttl => {
                    let idle = now.saturating_duration_since(held.idle_since);
                    (Some(held.handover), idle)
                }
                Some(stale) => {
                    drop(shared);
                    core.retire(profile, stale.handover, "ttl_expired");
                    (None, Duration::ZERO)
                }
                None => (None, Duration::ZERO),
            }
        };
        if lent.is_some() {
            core.lock().note(
                Level::Info,
                "session_lent",
                format!("profile={profile} idle_s={}", idle.as_secs()),
            );
        }
        Some(Lease(Arc::new(Mutex::new(LeaseState {
            core: core.clone(),
            profile: profile.to_owned(),
            lent,
            idle,
            #[cfg(unix)]
            socket: None,
            phase: Phase::Unopened,
        }))))
    }

    /// End every session idle for the TTL. Called every turn.
    pub(crate) fn expire(&self) {
        let Some(core) = &self.core else {
            return;
        };
        let now = Instant::now();
        let expired: Vec<(String, Held)> = {
            let mut shared = core.lock();
            let ttl = shared.ttl;
            let names: Vec<String> = shared
                .held
                .iter()
                .filter(|(_, held)| now.saturating_duration_since(held.idle_since) >= ttl)
                .map(|(name, _)| name.clone())
                .collect();
            names
                .into_iter()
                .filter_map(|name| shared.held.remove(&name).map(|held| (name, held)))
                .collect()
        };
        for (profile, held) in expired {
            core.retire(&profile, held.handover, "ttl_expired");
        }
    }

    /// The service is stopping: end every kept session now, all at once, and return when they are
    /// ended or [`LOGOUT_TIMEOUT`] has passed, so an unanswering NAS cannot stretch the stop by a
    /// logout per profile. One still unanswered at the bound is named.
    pub(crate) fn close(&self) {
        let Some(core) = &self.core else {
            return;
        };
        let held = std::mem::take(&mut core.lock().held);
        let (done, ended) = std::sync::mpsc::channel();
        let mut pending = Vec::new();
        for (profile, held) in held {
            let worker = {
                let core = core.clone();
                let profile = profile.clone();
                let done = done.clone();
                move || {
                    core.log_out_now(&profile, &held.handover, "service_stop");
                    let _ = done.send(profile);
                }
            };
            match std::thread::Builder::new()
                .name("sdsyncd-logout".into())
                .spawn(worker)
            {
                Ok(_) => pending.push(profile),
                Err(error) => core.lock().note(
                    Level::Warn,
                    "session_logout_failed",
                    format!(
                        "profile={profile} why=service_stop cause=no logout thread could be started: {error}; DSM ends the session on its own idle timer"
                    ),
                ),
            }
        }
        drop(done);
        let deadline = Instant::now() + LOGOUT_TIMEOUT + Duration::from_secs(1);
        while !pending.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            match ended.recv_timeout(left) {
                Ok(profile) => pending.retain(|name| *name != profile),
                Err(_) => break,
            }
        }
        for profile in pending {
            core.lock().note(
                Level::Warn,
                "session_logout_failed",
                format!(
                    "profile={profile} why=service_stop cause=DSM did not answer within {} s; DSM ends the session on its own idle timer",
                    LOGOUT_TIMEOUT.as_secs()
                ),
            );
        }
    }

    /// The notes since the last call, for the controller log.
    pub(crate) fn notes(&self) -> Vec<Note> {
        let Some(core) = &self.core else {
            return Vec::new();
        };
        let mut shared = core.lock();
        let mut notes = std::mem::take(&mut shared.notes);
        let dropped = std::mem::take(&mut shared.dropped_notes);
        if dropped > 0 {
            notes.push(Note {
                level: Level::Warn,
                event: "session_notes_dropped",
                detail: format!("count={dropped}"),
            });
        }
        notes
    }

    /// How many profiles have a kept session (tests and status only; never the sessions).
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.core.as_ref().map_or(0, |core| core.lock().held.len())
    }

    #[cfg(test)]
    pub(crate) fn ttl(&self) -> Option<Duration> {
        self.core.as_ref().map(|core| core.lock().ttl)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Not yet given to a child: dropping it keeps the lent session.
    Unopened,
    /// The child has its end; the handback is still to be read.
    Opened,
    Settled,
}

struct LeaseState {
    core: Core,
    profile: String,
    lent: Option<SessionHandover>,
    idle: Duration,
    #[cfg(unix)]
    socket: Option<std::os::unix::net::UnixStream>,
    phase: Phase,
}

/// One run's session channel. Clone shares it: the run spec is cloned, the channel is not.
#[derive(Clone)]
pub(crate) struct Lease(Arc<Mutex<LeaseState>>);

impl std::fmt::Debug for Lease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        write!(
            formatter,
            "Lease({} lent={} {:?})",
            state.profile,
            state.lent.is_some(),
            state.phase
        )
    }
}

/// What a run handed back: what became of the lent session, and the session it now owns.
#[cfg_attr(not(unix), allow(dead_code))]
struct Handback {
    borrowed: String,
    session: Option<SessionHandover>,
}

impl Lease {
    fn lock(&self) -> MutexGuard<'_, LeaseState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a session is lent (otherwise the run logs in and hands its own back).
    #[cfg(test)]
    pub(crate) fn lends(&self) -> bool {
        self.lock().lent.is_some()
    }

    /// The child's standard input: one end of a fresh socket pair, with the lent session (or
    /// [`LENT_NOTHING`]) already written and this side's writing shut down. A pair that cannot be
    /// made is named, and the child gets `/dev/null`: the run then names the unusable channel and
    /// logs in and out itself.
    ///
    /// [`LENT_NOTHING`]: synology_drive_sync::api::LENT_NOTHING
    #[cfg(unix)]
    pub(crate) fn stdin(&self) -> std::process::Stdio {
        match self.open() {
            Some(theirs) => std::process::Stdio::from(std::os::fd::OwnedFd::from(theirs)),
            None => std::process::Stdio::null(),
        }
    }

    /// The child's end of the channel, or `None` (named) when no channel could be made.
    #[cfg(unix)]
    fn open(&self) -> Option<std::os::unix::net::UnixStream> {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let mut state = self.lock();
        if state.phase != Phase::Unopened {
            return None;
        }
        let opened = UnixStream::pair().and_then(|(mut ours, theirs)| {
            // Written before the child exists, so it must fit the socket's buffer (a handover
            // is a few KiB of a buffer of hundreds); bounded, so it never stalls the run if not.
            ours.set_write_timeout(Some(Duration::from_secs(2)))?;
            // Bounds the handback's read too. Set here, before the write side is shut: macOS
            // refuses a socket option on a socket that has been shut down (EINVAL), which made
            // every handback there read as "the channel cannot be bounded".
            ours.set_read_timeout(Some(HANDBACK_WAIT))?;
            match &state.lent {
                Some(lent) => {
                    let encoded = lent.encode().map_err(std::io::Error::other)?;
                    ours.write_all(&encoded)?;
                }
                None => ours.write_all(synology_drive_sync::api::LENT_NOTHING)?,
            }
            ours.shutdown(std::net::Shutdown::Write)?;
            Ok((ours, theirs))
        });
        match opened {
            Ok((ours, theirs)) => {
                state.socket = Some(ours);
                state.phase = Phase::Opened;
                Some(theirs)
            }
            Err(error) => {
                let profile = state.profile.clone();
                state.core.lock().note(
                    Level::Warn,
                    "session_channel_failed",
                    format!("profile={profile} cause={error}; the run logs in itself"),
                );
                // The lent session never left: it stays kept for the next run.
                None
            }
        }
    }

    /// The run has ended: read what it handed back, keep that, and end what nobody holds any
    /// more. Idempotent; an unopened lease gives its session back to the cache untouched.
    pub(crate) fn settle(&self) {
        let mut state = self.lock();
        settle_state(&mut state);
    }
}

impl Drop for LeaseState {
    fn drop(&mut self) {
        settle_state(self);
    }
}

fn settle_state(state: &mut LeaseState) {
    match state.phase {
        Phase::Settled => {}
        Phase::Unopened => {
            state.phase = Phase::Settled;
            if let Some(lent) = state.lent.take() {
                restore(&state.core, &state.profile, lent);
            }
        }
        Phase::Opened => {
            state.phase = Phase::Settled;
            #[cfg(unix)]
            let handback = match state.socket.take() {
                Some(mut socket) => read_handback(&mut socket),
                None => Err("the channel was already closed".to_owned()),
            };
            #[cfg(not(unix))]
            let handback: Result<Handback, String> = Err("no channel on this platform".into());
            conclude(state, handback);
        }
    }
}

/// An unused lent session goes back as it was, unless a newer one arrived meanwhile.
fn restore(core: &Core, profile: &str, lent: SessionHandover) {
    let mut shared = core.lock();
    if shared.held.contains_key(profile) {
        drop(shared);
        core.retire(profile, lent, "replaced");
    } else {
        shared.held.insert(
            profile.to_owned(),
            Held {
                handover: lent,
                idle_since: Instant::now(),
            },
        );
    }
}

fn conclude(state: &mut LeaseState, handback: Result<Handback, String>) {
    let core = state.core.clone();
    let profile = state.profile.clone();
    let lent = state.lent.take();
    let handback = match handback {
        Ok(handback) => handback,
        Err(cause) => {
            core.lock().note(
                Level::Warn,
                "session_handback_missing",
                format!(
                    "profile={profile} cause={cause}; {}",
                    if lent.is_some() {
                        "the lent session is logged out"
                    } else {
                        "nothing is kept"
                    }
                ),
            );
            if let Some(lent) = lent {
                core.retire(&profile, lent, "not_handed_back");
            }
            return;
        }
    };
    let borrowed = handback.borrowed;
    let returned_lent = match (&lent, &handback.session) {
        (Some(lent), Some(session)) => lent.same_session(session),
        _ => false,
    };
    match handback.session {
        Some(session) => {
            core.lock().note(
                Level::Info,
                "session_kept",
                format!("profile={profile} borrowed={borrowed}"),
            );
            core.keep(&profile, session);
        }
        None => core.lock().note(
            Level::Info,
            "session_not_kept",
            format!("profile={profile} borrowed={borrowed}; the run ended its own session"),
        ),
    }
    let Some(lent) = lent else {
        return;
    };
    if returned_lent {
        return;
    }
    if borrowed == "rejected" {
        // DSM ended it already; shorten the TTL under the idle time it did not survive.
        let mut shared = core.lock();
        let halved = (state.idle / 2).max(MIN_TTL);
        if halved < shared.ttl {
            shared.ttl = halved;
            shared.note(
                Level::Warn,
                "session_ttl_shortened",
                format!(
                    "profile={profile} idle_s={} ttl_s={}; DSM refused a session idle that long",
                    state.idle.as_secs(),
                    halved.as_secs()
                ),
            );
        } else {
            shared.note(
                Level::Warn,
                "session_rejected",
                format!(
                    "profile={profile} idle_s={}; the run logged in once, afresh",
                    state.idle.as_secs()
                ),
            );
        }
        drop(lent);
        return;
    }
    core.retire(
        &profile,
        lent,
        if borrowed == "ignored" {
            "unused_by_run"
        } else {
            "replaced"
        },
    );
}

/// Read the handback: `sdsync.session-handback.v1 <borrowed>\n`, then at most one handover.
#[cfg(unix)]
fn read_handback(socket: &mut std::os::unix::net::UnixStream) -> Result<Handback, String> {
    use std::io::Read;

    use synology_drive_sync::api::MAX_HANDOVER_BYTES;
    use zeroize::Zeroizing;

    const STATUS_BYTES: usize = 128;
    // The read is bounded by the timeout `open` set on this end.
    let limit = STATUS_BYTES + MAX_HANDOVER_BYTES;
    let mut bytes = Zeroizing::new(Vec::with_capacity(limit + 1));
    let cut = match socket.take(limit as u64 + 1).read_to_end(&mut bytes) {
        Ok(_) => None,
        Err(error) if bytes.is_empty() => {
            return Err(format!("the run handed nothing back ({error})"));
        }
        // A run that closes with bytes of ours unread resets the pair instead of ending it; what
        // it wrote first is still whole, and the parse below proves it.
        Err(error) => Some(error),
    };
    if bytes.is_empty() {
        return Err("the run handed nothing back".into());
    }
    if bytes.len() > limit {
        return Err("the handback exceeds its bound".into());
    }
    parse_handback(&bytes).map_err(|cause| match cut {
        Some(error) => format!("the handback was cut short ({error})"),
        None => cause,
    })
}

#[cfg(unix)]
fn parse_handback(bytes: &[u8]) -> Result<Handback, String> {
    use synology_drive_sync::api::HANDBACK_SCHEMA;

    let Some(end) = bytes.iter().position(|byte| *byte == b'\n') else {
        return Err("the handback has no status line".into());
    };
    let status = std::str::from_utf8(&bytes[..end])
        .map_err(|_| "the handback's status line is not text".to_owned())?;
    let borrowed = match status.strip_prefix(HANDBACK_SCHEMA) {
        Some(rest) => match rest.strip_prefix(' ') {
            Some(word @ ("none" | "used" | "rejected" | "ignored")) => word.to_owned(),
            _ => return Err("the handback names no known outcome".into()),
        },
        None => return Err("the handback has an unknown schema".into()),
    };
    let rest = &bytes[end + 1..];
    let session = if rest.is_empty() {
        None
    } else {
        Some(SessionHandover::decode(rest).map_err(|error| error.to_string())?)
    };
    Ok(Handback { borrowed, session })
}

/// The TTL a daemon keeps sessions for. A debug build reads a test override, so the end-to-end
/// suite can watch a TTL lapse without waiting ten minutes; a release build never does.
pub(crate) fn configured_ttl() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(seconds) = std::env::var("SDSYNC_TEST_SESSION_TTL_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_secs(seconds.max(1));
    }
    IDLE_TTL
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use synology_drive_sync::api::{HANDBACK_SCHEMA, HANDOVER_SCHEMA, LENT_NOTHING};

    use super::*;

    const SID: &str = "unit-session-secret";
    const OTHER_SID: &str = "unit-other-session-secret";

    fn handover(sid: &str) -> SessionHandover {
        let document = serde_json::json!({
            "schema": HANDOVER_SCHEMA,
            "binding": "b".repeat(64),
            "base_url": "https://nas.invalid:5001",
            "allow_http": false,
            "accept_invalid_certs": false,
            "ca_certificate": null,
            "connect_timeout_ms": 1000,
            "request_timeout_ms": 1000,
            "retries": 0,
            "api_map": {"SYNO.API.Auth": {"path": "entry.cgi", "min": 1, "max": 7}},
            "sid": sid,
            "syno_token": null
        });
        SessionHandover::decode(document.to_string().as_bytes()).unwrap()
    }

    fn counting(ttl: Duration) -> (Sessions, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let sessions = Sessions::with_log_out(
            Some(ttl),
            Arc::new(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        );
        (sessions, count)
    }

    /// What the daemon lent, read the way the run reads it.
    fn lent(child: &mut UnixStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        child.read_to_end(&mut bytes).unwrap();
        bytes
    }

    fn hand_back(mut child: UnixStream, borrowed: &str, session: Option<&SessionHandover>) {
        child
            .write_all(format!("{HANDBACK_SCHEMA} {borrowed}\n").as_bytes())
            .unwrap();
        if let Some(session) = session {
            child.write_all(&session.encode().unwrap()).unwrap();
        }
        child.shutdown(std::net::Shutdown::Write).unwrap();
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A run of `profile` that hands back `back` with outcome `borrowed`; returns what it was lent.
    fn run(
        sessions: &Sessions,
        borrowed: &str,
        back: Option<&SessionHandover>,
    ) -> Option<SessionHandover> {
        let lease = sessions.lend("alpha").unwrap();
        let mut child = lease.open().unwrap();
        let bytes = lent(&mut child);
        hand_back(child, borrowed, back);
        lease.settle();
        (bytes != LENT_NOTHING).then(|| SessionHandover::decode(&bytes).unwrap())
    }

    fn events(sessions: &Sessions) -> Vec<&'static str> {
        sessions.notes().iter().map(|note| note.event).collect()
    }

    #[test]
    fn reuse_is_off_without_a_ttl() {
        assert!(Sessions::new(None).lend("alpha").is_none());
    }

    #[test]
    fn the_first_run_is_lent_nothing_and_its_session_is_kept() {
        let (sessions, logouts) = counting(IDLE_TTL);
        assert!(run(&sessions, "none", Some(&handover(SID))).is_none());
        assert_eq!(sessions.held(), 1);
        assert_eq!(events(&sessions), ["session_kept"]);
        assert_eq!(logouts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_kept_session_is_lent_to_the_next_run_and_comes_back() {
        let (sessions, logouts) = counting(IDLE_TTL);
        run(&sessions, "none", Some(&handover(SID)));
        let lent = run(&sessions, "used", Some(&handover(SID))).expect("lent");
        assert!(lent.same_session(&handover(SID)));
        assert_eq!(sessions.held(), 1);
        assert_eq!(
            logouts.load(Ordering::SeqCst),
            0,
            "a returned session is not ended"
        );
    }

    #[test]
    fn a_lease_never_given_to_a_child_keeps_its_session() {
        let (sessions, logouts) = counting(IDLE_TTL);
        run(&sessions, "none", Some(&handover(SID)));
        let lease = sessions.lend("alpha").unwrap();
        assert!(lease.lends());
        assert_eq!(sessions.held(), 0, "lent out");
        drop(lease);
        assert_eq!(sessions.held(), 1, "back, untouched");
        assert_eq!(logouts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_run_that_hands_nothing_back_has_the_lent_session_ended() {
        let (sessions, logouts) = counting(IDLE_TTL);
        run(&sessions, "none", Some(&handover(SID)));
        let lease = sessions.lend("alpha").unwrap();
        let child = lease.open().unwrap();
        drop(child); // the run died before its handback
        let started = Instant::now();
        lease.settle();
        assert!(started.elapsed() < HANDBACK_WAIT, "EOF, not the wait");
        wait_for("the logout", || logouts.load(Ordering::SeqCst) == 1);
        assert_eq!(sessions.held(), 0);
        wait_for("the logout note", || {
            sessions
                .notes()
                .iter()
                .any(|note| note.event == "session_logged_out")
        });
    }

    #[test]
    fn a_damaged_handback_is_named_and_the_lent_session_ended() {
        let (sessions, logouts) = counting(IDLE_TTL);
        run(&sessions, "none", Some(&handover(SID)));
        sessions.notes();
        let lease = sessions.lend("alpha").unwrap();
        let mut child = lease.open().unwrap();
        child
            .write_all(b"sdsync.session-handback.v9 used\n")
            .unwrap();
        drop(child);
        lease.settle();
        assert!(events(&sessions).contains(&"session_handback_missing"));
        wait_for("the logout", || logouts.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn a_refused_session_is_not_logged_out_and_the_ttl_shrinks() {
        let (sessions, logouts) = counting(IDLE_TTL);
        run(&sessions, "none", Some(&handover(SID)));
        sessions.notes();
        run(&sessions, "rejected", Some(&handover(OTHER_SID)));
        assert_eq!(sessions.ttl(), Some(MIN_TTL));
        assert_eq!(
            events(&sessions),
            ["session_lent", "session_kept", "session_ttl_shortened"]
        );
        assert_eq!(sessions.held(), 1, "the run's fresh session is kept");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(logouts.load(Ordering::SeqCst), 0, "DSM ended it already");
    }

    #[test]
    fn a_session_the_run_did_not_use_is_logged_out_and_its_own_kept() {
        let (sessions, logouts) = counting(IDLE_TTL);
        run(&sessions, "none", Some(&handover(SID)));
        let lent = run(&sessions, "ignored", Some(&handover(OTHER_SID))).unwrap();
        assert!(lent.same_session(&handover(SID)));
        wait_for("the logout", || logouts.load(Ordering::SeqCst) == 1);
        let next = run(&sessions, "used", Some(&handover(OTHER_SID))).unwrap();
        assert!(next.same_session(&handover(OTHER_SID)));
    }

    #[test]
    fn a_run_that_keeps_nothing_leaves_nothing_kept() {
        let (sessions, logouts) = counting(IDLE_TTL);
        run(&sessions, "none", None);
        assert_eq!(sessions.held(), 0);
        assert_eq!(events(&sessions), ["session_not_kept"]);
        assert_eq!(logouts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_idle_session_is_ended_at_the_ttl_and_never_lent() {
        let (sessions, logouts) = counting(Duration::ZERO);
        run(&sessions, "none", Some(&handover(SID)));
        assert!(run(&sessions, "none", None).is_none(), "stale: not lent");
        wait_for("the stale session's logout", || {
            logouts.load(Ordering::SeqCst) == 1
        });
        run(&sessions, "none", Some(&handover(SID)));
        sessions.expire();
        wait_for("the expiry", || logouts.load(Ordering::SeqCst) == 2);
        assert_eq!(sessions.held(), 0);
    }

    #[test]
    fn stopping_ends_every_kept_session_before_it_returns() {
        let (sessions, logouts) = counting(IDLE_TTL);
        run(&sessions, "none", Some(&handover(SID)));
        let lease = sessions.lend("beta").unwrap();
        let child = lease.open().unwrap();
        hand_back(child, "none", Some(&handover(OTHER_SID)));
        // The run never read what it was lent (a reset, not an end): its handback still counts.
        lease.settle();
        assert_eq!(sessions.held(), 2, "{:?}", sessions.notes());
        sessions.close();
        assert_eq!(logouts.load(Ordering::SeqCst), 2, "synchronously");
        assert_eq!(sessions.held(), 0);
    }

    #[test]
    fn a_failed_logout_is_named() {
        let sessions = Sessions::with_log_out(
            Some(IDLE_TTL),
            Arc::new(|_| Err("DSM did not answer".to_owned())),
        );
        run(&sessions, "none", Some(&handover(SID)));
        sessions.notes();
        sessions.close();
        let notes = sessions.notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].event, "session_logout_failed");
        assert!(notes[0].detail.contains("DSM did not answer"), "{notes:?}");
    }

    #[test]
    fn no_note_and_no_debug_line_ever_carries_the_session() {
        let (sessions, _) = counting(Duration::from_secs(600));
        let mut text = String::new();
        run(&sessions, "none", Some(&handover(SID)));
        let lease = sessions.lend("alpha").unwrap();
        text.push_str(&format!("{lease:?}"));
        let mut child = lease.open().unwrap();
        let bytes = lent(&mut child);
        assert!(
            String::from_utf8_lossy(&bytes).contains(SID),
            "the channel does carry it"
        );
        hand_back(child, "rejected", Some(&handover(OTHER_SID)));
        lease.settle();
        text.push_str(&format!("{lease:?}"));
        sessions.close();
        for note in sessions.notes() {
            text.push_str(&format!("{note:?}\n"));
        }
        assert!(!text.contains(SID) && !text.contains(OTHER_SID), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }
}
