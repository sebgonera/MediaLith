//! Dropping root, in the one order that actually drops it.
//!
//! ADR-0007 runs Plex as an unprivileged user. Getting there from PID 1's child means
//! three syscalls, and the order is not a style question: done wrongly the process
//! keeps the ability to undo the whole thing, and every symptom of that is invisible
//! until something exploits it.
//!
//! # The order, and why each step has to be where it is
//!
//! 1. `setgroups([])` — drop supplementary groups. Must come **first**, because it
//!    needs privilege the later calls give away. Skipping it is the classic hole: a
//!    process that has dropped to an unprivileged uid while still carrying root's
//!    supplementary groups has whatever those groups can reach, and `id` prints them
//!    plainly to anyone who thinks to look.
//! 2. `setgid()` — the primary group. Before `setuid`, for the same reason: after
//!    `setuid` there is no privilege left to change groups with, and the call fails
//!    silently enough to miss.
//! 3. `setuid()` — last, and irreversible. A process that calls `setuid` from root to
//!    a non-zero uid cannot return; that is the property being bought.
//!
//! # Verifying rather than trusting
//!
//! [`drop_to`] reads the ids back afterwards and fails if they are not what was asked
//! for. `setuid` returning 0 is not proof on every path — most famously when the
//! target uid is already the current one and the call is a no-op — and a confinement
//! that quietly did nothing is the failure this module exists to prevent.

use std::io;

/// Becomes `uid`:`gid` and gives up every way back.
///
/// # Errors
/// If any step fails, or if the ids read back afterwards are not the ones requested.
/// Every failure here means the caller is still privileged and must not continue as
/// though it were not.
pub fn drop_to(uid: u32, gid: u32) -> io::Result<()> {
    if uid == 0 || gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to \"drop\" privileges to {uid}:{gid}. Dropping to root is not \
                 dropping anything, and a caller asking for it has computed the wrong \
                 target rather than meant it."
            ),
        ));
    }

    // SAFETY: setgroups with a count of zero reads no memory; the pointer is required
    // to be valid only when the count is non-zero, and null with zero is the documented
    // way to clear the list.
    let cleared = unsafe { libc::setgroups(0, std::ptr::null()) };
    if cleared != 0 {
        return Err(annotate(
            &io::Error::last_os_error(),
            "could not drop supplementary groups. Continuing would leave the process \
             carrying root's group memberships under an unprivileged uid, which looks \
             confined and is not",
        ));
    }

    // SAFETY: a scalar syscall. No pointers involved.
    if unsafe { libc::setgid(gid) } != 0 {
        return Err(annotate(
            &io::Error::last_os_error(),
            &format!("could not change to gid {gid}"),
        ));
    }

    // SAFETY: a scalar syscall. Irreversible from root to a non-zero uid, which is the
    // property being bought.
    if unsafe { libc::setuid(uid) } != 0 {
        return Err(annotate(
            &io::Error::last_os_error(),
            &format!("could not change to uid {uid}"),
        ));
    }

    confirm(uid, gid)
}

/// Reads the ids back and refuses to agree that they changed unless they did.
///
/// Separate from the calls above so the check cannot be skipped by a future edit that
/// adds a step in between.
fn confirm(uid: u32, gid: u32) -> io::Result<()> {
    // SAFETY: getuid takes no arguments and cannot fail; POSIX gives it no error
    // return at all. The same holds for the three below.
    let user_real = unsafe { libc::getuid() };
    // SAFETY: as above.
    let user_effective = unsafe { libc::geteuid() };
    // SAFETY: as above.
    let group_real = unsafe { libc::getgid() };
    // SAFETY: as above.
    let group_effective = unsafe { libc::getegid() };

    if user_real != uid || user_effective != uid {
        return Err(io::Error::other(format!(
            "asked to become uid {uid} and the process is {user_real} real, \
             {user_effective} effective. Privileges were not dropped."
        )));
    }
    if group_real != gid || group_effective != gid {
        return Err(io::Error::other(format!(
            "asked to become gid {gid} and the process is {group_real} real, \
             {group_effective} effective. Privileges were not dropped."
        )));
    }
    Ok(())
}

/// Whether this process is root.
#[must_use]
pub fn is_root() -> bool {
    // SAFETY: geteuid takes no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Adds context to an errno without losing its kind.
fn annotate(error: &io::Error, context: &str) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_to_root_is_refused_because_it_drops_nothing() {
        // The realistic way this gets called with 0: a lookup that failed and returned
        // a default. Succeeding would leave a process running as root that every log
        // line claims is confined.
        let error = drop_to(0, 0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("not dropping anything"),
            "{error}"
        );

        assert!(drop_to(900, 0).is_err(), "a root gid is no better");
        assert!(drop_to(0, 900).is_err());
    }

    #[test]
    fn the_target_is_the_uid_plexos_types_froze() {
        // The two have to agree: this drops to a uid, and the Buildroot users table
        // creates one. A mismatch hands Plex a data directory it does not own, and the
        // error names a file rather than an account.
        assert_eq!(plexos_types::paths::PLEX_UID, 900);
        assert_eq!(plexos_types::paths::PLEX_GID, 900);
    }

    #[test]
    fn a_failed_drop_is_detectable_after_the_fact() {
        // confirm() is what turns "the syscall returned 0" into "the process really is
        // unprivileged". Running as a normal user, dropping to that same user is a
        // no-op that every syscall reports as success -- exactly the case where a
        // return value proves nothing and reading the ids back does.
        // SAFETY: getuid takes no arguments and cannot fail.
        let uid = unsafe { libc::getuid() };
        // SAFETY: as above, for getgid.
        let gid = unsafe { libc::getgid() };
        if uid == 0 {
            return; // the interesting case needs a non-root test runner
        }
        assert!(confirm(uid, gid).is_ok(), "the ids we already have");
        assert!(confirm(uid + 1, gid).is_err(), "and not the ones we do not");
    }
}
