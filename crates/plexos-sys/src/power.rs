//! Turning the machine off, and doing it without losing the media database.
//!
//! Holding the power button for five seconds cuts power mid-write. XFS journals, so
//! `/var` will usually come back — but "usually" is doing real work in that sentence, and
//! what is on `/var` is the thing a user would actually miss: their library, their watch
//! history, the app image they waited two minutes to build. An appliance with no keyboard
//! and no shell has to offer a way to stop that is not the power button.
//!
//! # The order, and why each step is where it is
//!
//! 1. **Stop what is writing.** Plex is asked to exit and given time to close its
//!    database. A database killed with `SIGKILL` mid-write is the specific damage this
//!    module exists to avoid.
//! 2. **`sync`.** Everything still in the page cache reaches the disk.
//! 3. **Remount `/var` read-only.** This is the step that turns "probably fine" into
//!    "nothing was in flight". It fails harmlessly if something still holds a file open,
//!    and the caller reports that rather than pretending.
//! 4. **`reboot(2)`.** Which never returns.
//!
//! `/usr` needs nothing: it is a read-only dm-verity mount. The tmpfs root is discarded
//! by definition.
//!
//! # What has run
//!
//! **None of this has been executed.** `reboot(2)` is not a call to try casually on the
//! machine you are working on. Delete this notice when a machine has been turned off by
//! it.

use std::io;
use std::path::Path;

/// How long Plex is given to shut down before the machine goes anyway.
///
/// Ten seconds. Plex closes its database in well under that; a hung one must not leave
/// somebody holding a power button after all, which is the situation being replaced.
pub const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Flushes every filesystem buffer to disk.
///
/// Cannot fail: `sync(2)` returns nothing and waits for the writes to reach the device.
pub fn sync() {
    // SAFETY: sync() takes no arguments, returns nothing, and has no failure mode. It
    // cannot violate any invariant of this process.
    unsafe { libc::sync() };
}

/// Asks a process to exit.
///
/// `SIGTERM`, not `SIGKILL`: the point is to let it close what it has open.
///
/// # Errors
/// If the signal cannot be sent, which in practice means the process has already gone —
/// that is worth reporting rather than hiding, because "Plex was already dead" and "Plex
/// ignored the signal" lead to different conclusions about a machine.
pub fn terminate(pid: u32) -> io::Result<()> {
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid does not fit in a pid_t"))?;

    // SAFETY: kill() with a valid signal number is safe for any pid; an invalid or
    // vanished pid is reported as ESRCH rather than being undefined. Nothing in this
    // process's memory is touched.
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Remounts a filesystem read-only, so nothing can be in flight when power goes.
///
/// # Errors
/// `EBUSY` if a process still holds a file open for writing. That is not fatal to a
/// shutdown and the caller should say so rather than abandon it: `sync` has already run,
/// and a journalling filesystem recovers from what remains.
pub fn remount_read_only(target: &Path) -> io::Result<()> {
    /// `MS_REMOUNT`, from `linux/fs.h`. Not in [`crate::mount::flags`] because nothing
    /// else needs it, and a flag with one caller is clearer next to that caller.
    const REMOUNT: u64 = 32;

    let c_target = std::ffi::CString::new(target.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))?;

    // SAFETY: mount() is called with a valid NUL-terminated target and null pointers for
    // source, fstype and data, which is what MS_REMOUNT accepts -- the remaining
    // arguments are ignored for a remount. The pointer is valid for the duration of the
    // call because c_target outlives it.
    let result = unsafe {
        libc::mount(
            std::ptr::null(),
            c_target.as_ptr(),
            std::ptr::null(),
            (REMOUNT | crate::mount::flags::RDONLY) as libc::c_ulong,
            std::ptr::null(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// What to do to the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Cut the power.
    Off,
    /// Restart.
    Restart,
}

impl Action {
    /// The `reboot(2)` command for this action.
    #[must_use]
    fn command(self) -> libc::c_int {
        match self {
            Self::Off => libc::RB_POWER_OFF,
            Self::Restart => libc::RB_AUTOBOOT,
        }
    }

    /// How this reads in a diagnostic.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Off => "power off",
            Self::Restart => "restart",
        }
    }
}

/// Stops the machine. Does not return when it works.
///
/// Call [`sync`] first. This does not do it, because the caller has other things to
/// finish — stopping Plex, remounting `/var` — and a sync buried in here would run at the
/// wrong moment and read as though the ordering were handled.
///
/// # Errors
/// `EPERM` when not running as root, which is the only realistic failure. Anything else
/// means the kernel refused, and the caller has no fallback but to say so: an appliance
/// that says it is shutting down and does not is worse than one that admits it cannot.
pub fn stop(action: Action) -> io::Result<std::convert::Infallible> {
    // SAFETY: reboot() takes an integer command and, on success, does not return. Both
    // commands used here are the constants the kernel defines. Failure is reported
    // through errno like any other syscall, and touches nothing in this process.
    unsafe { libc::reboot(action.command()) };
    Err(io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_actions_map_to_the_kernels_own_constants() {
        // Pinned against libc's definitions rather than against numbers written here: a
        // transposed pair would reboot a machine somebody asked to switch off, which is
        // the one mistake in this module that is not merely inconvenient.
        assert_eq!(Action::Off.command(), libc::RB_POWER_OFF);
        assert_eq!(Action::Restart.command(), libc::RB_AUTOBOOT);
        assert_ne!(Action::Off.command(), Action::Restart.command());
    }

    #[test]
    fn each_action_says_what_it_does() {
        assert_eq!(Action::Off.describe(), "power off");
        assert_eq!(Action::Restart.describe(), "restart");
    }

    #[test]
    fn syncing_is_safe_to_call_here() {
        // It has no failure mode and no arguments; calling it in a test is the cheapest
        // way to prove the linkage is right rather than merely declared.
        sync();
    }

    #[test]
    fn terminating_a_pid_that_cannot_exist_is_an_error_rather_than_silence() {
        // "Plex was already dead" and "Plex ignored the signal" lead to different
        // conclusions, so this must not quietly succeed.
        let result = terminate(u32::MAX - 1);
        assert!(result.is_err(), "a vanished process must be reported");
    }

    #[test]
    fn remounting_something_that_is_not_a_mount_point_fails_rather_than_appearing_to_work() {
        let error = remount_read_only(Path::new("/nonexistent-plexos-mount")).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ),
            "unexpected: {error}"
        );
    }

    #[test]
    fn the_grace_period_is_long_enough_to_close_a_database_and_short_enough_to_wait_for() {
        assert!(
            GRACE.as_secs() >= 5,
            "SQLite needs a moment to close cleanly"
        );
        assert!(
            GRACE.as_secs() <= 30,
            "longer than this and somebody reaches for the power button anyway, which is \
             the situation this replaces"
        );
    }
}
