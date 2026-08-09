//! Setting the system clock, for the one failure an appliance cannot talk its way out of.
//!
//! Nothing in this image synchronises time. There is no NTP client, no chrony, no
//! `sntp` — the clock is whatever the platform handed the kernel at boot, and on x86
//! that is the CMOS clock, read by `mach_get_cmos_time()` through
//! `read_persistent_clock64()` regardless of `CONFIG_RTC_HCTOSYS`.
//!
//! Which is fine until the battery dies. Then the machine believes it is 2010, or 1970,
//! and **every outbound TLS handshake fails** — the certificate the far end presents is
//! not yet valid, because from here it has not been issued yet. That is what stops Plex
//! being downloaded, and the message says `certificate is not yet valid or the system
//! clock is incorrect`, which names the cause and is still read as a problem with the
//! server.
//!
//! The console's own certificate is deliberately valid from 1975 to 4096 so that a dead
//! battery cannot take the console away too (see `plexosd::tls`). That protects the way
//! *in*; this protects the way *out*.
//!
//! # What has run
//!
//! **Nothing here has run on hardware.** `clock_settime` is one syscall with one failure
//! mode, and the caller that uses it is covered by tests, but neither has yet run on a
//! machine with a wrong clock.

use std::io;

/// Sets `CLOCK_REALTIME` to `seconds` since the Unix epoch.
///
/// Only the seconds are set. Sub-second accuracy is meaningless here: the value this is
/// ever called with comes from a build stamp with minute resolution, and the point is to
/// leave the clock plausible rather than correct.
///
/// # Errors
///
/// `EPERM` if the caller lacks `CAP_SYS_TIME`, and `EINVAL` if `seconds` is outside what
/// the kernel accepts. Both are reported rather than ignored, because a clock that
/// silently stayed wrong would reproduce the failure this exists to prevent.
pub fn set_realtime(seconds: i64) -> io::Result<()> {
    let tv_sec = libc::time_t::try_from(seconds).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{seconds} does not fit this platform's time_t"),
        )
    })?;
    let when = libc::timespec { tv_sec, tv_nsec: 0 };

    // SAFETY: `when` is a fully initialised `timespec` that outlives the call, and
    // `clock_settime` reads it and returns without retaining the pointer. CLOCK_REALTIME
    // is settable by definition, so the only failures are the errno cases above.
    let result = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &raw const when) };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The system clock now, in seconds since the epoch.
///
/// Negative if the clock is set before 1970, which is a state this module exists to
/// notice rather than one to treat as impossible.
#[must_use]
pub fn realtime_now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_reads_as_a_time_after_this_was_written() {
        // Not a tautology: it catches a `realtime_now` that returns zero, or seconds
        // where it meant nanoseconds, on any machine whose own clock is sane. 2026-01-01.
        assert!(
            realtime_now() > 1_767_225_600,
            "the build host's own clock reads as before 2026, which makes every other \
             assertion here meaningless"
        );
    }

    #[test]
    fn setting_the_clock_without_privilege_reports_rather_than_pretends() {
        // Run as an ordinary user this is EPERM, and that is the point: the function
        // must not return Ok having changed nothing. If this ever runs as root it would
        // set the clock, so it asks for the time the clock already has.
        let result = set_realtime(realtime_now());
        match result {
            Err(error) => assert_eq!(
                error.kind(),
                io::ErrorKind::PermissionDenied,
                "expected EPERM as an ordinary user, got {error}"
            ),
            Ok(()) => assert_eq!(
                nix_uid(),
                0,
                "setting the clock succeeded without being root, which means the call \
                 did not reach the kernel"
            ),
        }
    }

    fn nix_uid() -> u32 {
        // SAFETY: getuid() takes no arguments, cannot fail, and returns a plain integer.
        unsafe { libc::getuid() }
    }
}
