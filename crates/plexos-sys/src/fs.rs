//! How much room is left on a filesystem.
//!
//! The one number about `/var` that nothing on this appliance could answer. Everything
//! else it reports about itself comes out of `/proc` or `/sys` as text; free space does
//! not, because there is no file that holds it — `statvfs(3)` asks the filesystem and no
//! kernel interface exposes the answer any other way.
//!
//! # Why it is worth a syscall
//!
//! `/var` is the only partition anything writes to, it holds the media database, and its
//! largest writer — Plex's transcode scratch — is bounded by nothing. The ESP has already
//! demonstrated what an unwatched partition does when it reaches 100%: an update failed
//! part-way through copying and left a truncated kernel that could not boot. `/var` is the
//! same story with a database on it.
//!
//! So this exists for two callers. The dashboard shows the number to a person, and the
//! update and provisioning paths can refuse a download that will not fit — which today
//! they cannot, because they have no way to ask.
//!
//! # What has run
//!
//! **Nothing on the appliance.** The syscall is exercised by tests against the build
//! host's own filesystems, where the answers can be compared with `df`.

use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

/// What a filesystem has and what is left of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// Total size in bytes.
    pub total: u64,
    /// Bytes an unprivileged process may still write.
    ///
    /// `f_bavail`, not `f_bfree`. They differ by the reserve most filesystems keep for
    /// root, and reporting the larger one tells a person they have room that the process
    /// which actually writes there — Plex, as uid 900 — cannot use.
    pub available: u64,
}

impl Space {
    /// Bytes in use, which is total minus what is free *to anyone*.
    ///
    /// Derived rather than stored so it cannot disagree with the two fields it comes
    /// from. Saturating because a filesystem may report an available count above its own
    /// total — network filesystems do, and an appliance that panicked over an odd `df`
    /// would be worse than one that showed zero.
    #[must_use]
    pub const fn used(self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    /// How full it is, 0 to 100, or `None` for a filesystem of no size.
    ///
    /// `None` rather than zero: `/proc` and friends report a total of zero, and "0% full"
    /// about them is a number that means nothing dressed as one that means something.
    #[must_use]
    pub fn percent_used(self) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "at f64's 53-bit mantissa this is exact to the byte up to 8 PiB, and \
                      beyond that the rounding is smaller than one part in a quadrillion \
                      of a percentage that is drawn as a bar"
        )]
        Some((self.used() as f64 / self.total as f64) * 100.0)
    }
}

/// Asks a filesystem how much room it has, through any path on it.
///
/// # Errors
/// `ENOENT` for a path that does not exist, `EACCES` if a directory on the way cannot be
/// searched. Both are worth reporting rather than rendering as zero, which is a number a
/// reader would believe.
pub fn space(path: &Path) -> io::Result<Space> {
    let mut raw = std::mem::MaybeUninit::<libc::statvfs>::uninit();

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;

    // SAFETY: statvfs() reads the filesystem containing the named path and writes one
    // `struct statvfs` through the pointer. The path is a NUL-terminated C string that
    // outlives the call, and the pointer is to a correctly sized, correctly aligned
    // MaybeUninit that this function owns. Nothing else aliases it. On failure the kernel
    // writes nothing, which is why the result is only assumed initialised below.
    let result = unsafe { libc::statvfs(c_path.as_ptr(), raw.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: statvfs() returned 0, which is the contract that it filled the structure.
    let raw = unsafe { raw.assume_init() };

    // f_frsize, not f_bsize. f_bsize is a hint about efficient I/O size; f_frsize is the
    // unit the block counts are in, and on filesystems where they differ, using the wrong
    // one is a total that is wrong by that ratio and still looks entirely plausible.
    let unit = raw.f_frsize;

    Ok(Space {
        total: unit.saturating_mul(raw.f_blocks),
        available: unit.saturating_mul(raw.f_bavail),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_filesystem_answers_with_something_plausible() {
        // Against the host this runs on, because the whole point of the module is the one
        // number that cannot be read out of a file and therefore cannot be faked with a
        // fixture.
        let space = space(Path::new("/")).expect("/ is on a filesystem");

        assert!(space.total > 0, "a root filesystem has a size");
        assert!(
            space.available <= space.total,
            "available {} exceeds total {}",
            space.available,
            space.total
        );
        assert!(
            space
                .percent_used()
                .is_some_and(|p| (0.0..=100.0).contains(&p)),
            "a percentage outside 0..100 is arithmetic gone wrong, not a full disk"
        );
    }

    #[test]
    fn the_answer_is_about_the_filesystem_not_the_path() {
        // Any path on a filesystem gives the same answer, which is what makes it safe to
        // ask about a directory that happens to exist rather than a mount point.
        let root = space(Path::new("/")).expect("/");
        let etc = space(Path::new("/etc")).expect("/etc");

        // Sizes, not free space: something may write between the two calls.
        assert_eq!(
            root.total, etc.total,
            "/ and /etc are the same filesystem on any host this runs on"
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_an_error_rather_than_zero() {
        // Zero would render as a full disk, and a full disk is the alarm this exists to
        // raise. Reporting one because a path was misspelled would be the worst kind of
        // wrong: alarming, and about nothing.
        let error = space(Path::new("/no/such/path/on/any/machine")).expect_err("no such path");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_filesystem_of_no_size_has_no_percentage() {
        let nothing = Space {
            total: 0,
            available: 0,
        };
        assert_eq!(nothing.percent_used(), None);
    }

    #[test]
    fn used_is_derived_and_cannot_go_negative() {
        let odd = Space {
            total: 100,
            available: 150,
        };
        assert_eq!(
            odd.used(),
            0,
            "an odd df must not underflow into 18 exabytes"
        );
    }
}
