//! Every `unsafe` call the daemon makes, each a thin wrapper over one libc
//! function with its contract stated. Linux only.

use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use super::window::{LocalClock, WallFields};

/// The signals the daemon takes as messages: TERM/INT stop, HUP reload, USR2 wake.
pub(crate) const DAEMON_SIGNALS: [libc::c_int; 4] =
    [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGUSR2];

fn signal_set(signals: &[libc::c_int]) -> io::Result<libc::sigset_t> {
    let mut set = MaybeUninit::<libc::sigset_t>::zeroed();
    // SAFETY: sigemptyset/sigaddset initialise and update the set they are given.
    unsafe {
        if libc::sigemptyset(set.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        for signal in signals {
            if libc::sigaddset(set.as_mut_ptr(), *signal) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(set.assume_init())
    }
}

/// Block the daemon signals in the calling thread. Called before any thread is
/// spawned, so every thread inherits the mask and only `wait_signal` sees them:
/// no handlers, no latches.
pub(crate) fn block_daemon_signals() -> io::Result<()> {
    let set = signal_set(&DAEMON_SIGNALS)?;
    // SAFETY: the set is initialised; a null old-set pointer is allowed.
    let status = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status))
    }
}

/// `sigwait` on the daemon signals. Returns the signal number.
pub(crate) fn wait_signal() -> io::Result<libc::c_int> {
    let set = signal_set(&DAEMON_SIGNALS)?;
    let mut signal: libc::c_int = 0;
    // SAFETY: both pointers are valid for the call.
    let status = unsafe { libc::sigwait(&set, &mut signal) };
    if status == 0 {
        Ok(signal)
    } else {
        Err(io::Error::from_raw_os_error(status))
    }
}

/// Send `signal` to the process group `pgid` (I-10: only our own child's group).
pub(crate) fn kill_group(pgid: u32, signal: libc::c_int) -> io::Result<()> {
    let pgid = libc::pid_t::try_from(pgid)
        .ok()
        .filter(|pgid| *pgid > 1)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: kill has no pointer arguments; a negative pid names a group.
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // ESRCH: the group is already gone, which is what was wanted.
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

/// Send `signal` to one process.
pub(crate) fn kill_process(pid: u32, signal: libc::c_int) -> io::Result<()> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: kill has no pointer arguments.
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The child half of a spawn, run between fork and exec: its own process
/// group, death with the supervising thread, an empty signal mask (the daemon's
/// blocked set is inherited across fork and would make the core deaf to TERM),
/// and the performance level's nice value.
///
/// # Safety
/// Must only be called in the child between fork and exec. Every call here is
/// async-signal-safe and allocates nothing.
pub(crate) unsafe fn child_setup(parent: libc::pid_t, nice: Option<i32>) -> io::Result<()> {
    // SAFETY: as documented above; all async-signal-safe syscalls.
    unsafe {
        if libc::setpgid(0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::getppid() != parent {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        let mut empty = MaybeUninit::<libc::sigset_t>::zeroed();
        libc::sigemptyset(empty.as_mut_ptr());
        let status = libc::pthread_sigmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut());
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if let Some(nice) = nice
            && libc::setpriority(libc::PRIO_PROCESS, 0, nice) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn getpid() -> libc::pid_t {
    // SAFETY: no arguments, cannot fail.
    unsafe { libc::getpid() }
}

pub(crate) fn geteuid() -> u32 {
    // SAFETY: no arguments, cannot fail.
    unsafe { libc::geteuid() }
}

/// Set the process umask, returning the previous one.
pub(crate) fn set_umask(mask: u32) -> u32 {
    // SAFETY: no pointer arguments, cannot fail.
    unsafe { libc::umask(mask as libc::mode_t) as u32 }
}

/// `flock(LOCK_EX|LOCK_NB)`: `Ok(false)` when another process holds it. The
/// kernel releases it when the holder dies, so no stale-lock recovery exists.
pub(crate) fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: the descriptor is valid for the life of `file`.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// The connecting process's uid, from `SO_PEERCRED`.
pub(crate) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials = MaybeUninit::<libc::ucred>::zeroed();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the buffer and its length describe a ucred the kernel fills in.
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: getsockopt filled the whole structure.
    Ok(unsafe { credentials.assume_init() }.uid)
}

/// Local time from the C library (musl reads `TZ` or `/etc/localtime`).
/// `tzset` runs before each conversion so a zone change is picked up where the
/// C library supports it.
pub(crate) struct LibcLocalClock;

unsafe extern "C" {
    fn tzset();
}

impl LocalClock for LibcLocalClock {
    // time_t and long are 32-bit on some armv7 C libraries: the conversions
    // are only useless on 64-bit targets.
    #[allow(clippy::useless_conversion)]
    fn fields(&self, epoch: i64) -> Option<WallFields> {
        let time: libc::time_t = epoch.try_into().ok()?;
        let mut tm = MaybeUninit::<libc::tm>::zeroed();
        // SAFETY: tzset has no arguments; localtime_r writes into the buffer we own.
        let tm = unsafe {
            tzset();
            if libc::localtime_r(&time, tm.as_mut_ptr()).is_null() {
                return None;
            }
            tm.assume_init()
        };
        let weekday = if tm.tm_wday == 0 { 7 } else { tm.tm_wday };
        Some(WallFields {
            day: format!(
                "{:04}-{:02}-{:02}",
                i64::from(tm.tm_year) + 1900,
                tm.tm_mon + 1,
                tm.tm_mday
            ),
            weekday: u8::try_from(weekday).ok()?,
            minute: i64::from(tm.tm_hour) * 60 + i64::from(tm.tm_min),
            offset: i64::from(tm.tm_gmtoff) / 60,
        })
    }
}

/// The zone name the C library resolved, for the start-up log line (R9).
#[allow(clippy::useless_conversion)]
pub(crate) fn zone_name(epoch: i64) -> Option<String> {
    let time: libc::time_t = epoch.try_into().ok()?;
    let mut tm = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: as above; tm_zone points at static storage owned by the C library.
    unsafe {
        tzset();
        if libc::localtime_r(&time, tm.as_mut_ptr()).is_null() {
            return None;
        }
        let tm = tm.assume_init();
        if tm.tm_zone.is_null() {
            return None;
        }
        Some(CStr::from_ptr(tm.tm_zone).to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_c_library_clock_agrees_with_utc_arithmetic_under_tz_utc() {
        // Only meaningful when the test process runs with TZ unset or UTC, as the
        // container gate does; otherwise just check it answers.
        let fields = LibcLocalClock.fields(1_772_953_200).unwrap();
        assert!((1..=7).contains(&fields.weekday));
        if std::env::var("TZ").map_or(true, |tz| tz == "UTC" || tz.is_empty()) && fields.offset == 0
        {
            assert_eq!(
                fields,
                super::super::window::fields_at_offset(1_772_953_200, 0)
            );
        }
    }

    #[test]
    fn flock_is_exclusive_across_descriptions() {
        let path = std::env::temp_dir().join(format!("sdsyncd-flock-{}", std::process::id()));
        let first = File::create(&path).unwrap();
        let second = File::open(&path).unwrap();
        assert!(try_lock_exclusive(&first).unwrap());
        assert!(!try_lock_exclusive(&second).unwrap());
        drop(first);
        assert!(try_lock_exclusive(&second).unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn peer_credentials_name_our_own_uid() {
        let (left, _right) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&left).unwrap(), geteuid());
    }
}
