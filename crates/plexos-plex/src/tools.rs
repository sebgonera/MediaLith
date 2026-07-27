//! Locating the external programs provisioning needs, by absolute path.
//!
//! `Command::new("tar")` does not work here, and the way it fails is quiet. Provisioning
//! may run from a process started by PID 1, which inherits the empty environment the
//! kernel provides, so there is no `PATH`; glibc's `execvp` then falls back to
//! `confstr(_CS_PATH)`, which is `/bin:/usr/bin`. busybox and erofs-utils install into
//! `/sbin` and `/usr/sbin` as well, so a lookup that works when a person types it at a
//! shell — which sets its own `PATH` — fails from the daemon with a bare `ENOENT`.
//!
//! That cost a boot in `plexosd::net` before it was understood. Encoding it here means
//! the next thing to shell out inherits the answer rather than the bug.

use std::path::{Path, PathBuf};

/// Where to look, in order.
pub const PROGRAM_DIRS: [&str; 4] = ["/sbin", "/usr/sbin", "/bin", "/usr/bin"];

/// The programs a provisioning run cannot proceed without.
///
/// Resolved together, up front, so a missing one is reported before an 80 MB download
/// rather than after it — and before anything has been written to `/var`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    /// Unpacks `data.tar.xz`.
    pub tar: PathBuf,
    /// Builds the app image.
    pub mkfs_erofs: PathBuf,
    /// Computes the integrity record.
    pub sha256sum: PathBuf,
}

/// A program that should be in the image and is not.
#[derive(Debug, PartialEq, Eq)]
pub struct Missing {
    /// What was looked for.
    pub program: String,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is in none of {}. Plex cannot be provisioned without it. This is an \
             image fault rather than anything to do with the download: busybox and \
             erofs-utils are supposed to provide it.",
            self.program,
            PROGRAM_DIRS.join(", ")
        )
    }
}

impl std::error::Error for Missing {}

/// Finds one program, ignoring `PATH` entirely.
#[must_use]
pub fn resolve(program: &str, exists: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    PROGRAM_DIRS.iter().find_map(|dir| {
        let candidate = PathBuf::from(dir).join(program);
        exists(&candidate).then_some(candidate)
    })
}

impl Tools {
    /// Resolves every program, reporting the first that is absent.
    ///
    /// # Errors
    /// [`Missing`] when one is not installed.
    pub fn find(exists: &dyn Fn(&Path) -> bool) -> Result<Self, Missing> {
        let one = |program: &str| {
            resolve(program, exists).ok_or_else(|| Missing {
                program: program.to_owned(),
            })
        };
        Ok(Self {
            tar: one("tar")?,
            mkfs_erofs: one("mkfs.erofs")?,
            sha256sum: one("sha256sum")?,
        })
    }

    /// Resolves against the running system.
    ///
    /// # Errors
    /// [`Missing`] when one is not installed.
    pub fn on_this_system() -> Result<Self, Missing> {
        Self::find(&|p: &Path| p.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_search_directory_is_absolute() {
        for dir in PROGRAM_DIRS {
            assert!(dir.starts_with('/'), "{dir} would be resolved through PATH");
        }
    }

    #[test]
    fn the_sbin_directories_come_first_because_that_is_where_busybox_installs() {
        // The glibc fallback covers /bin and /usr/bin only. Searching those first would
        // still work, but putting sbin first documents which of the four is the reason
        // this module exists at all.
        assert_eq!(PROGRAM_DIRS[0], "/sbin");
        assert_eq!(PROGRAM_DIRS[1], "/usr/sbin");
    }

    #[test]
    fn a_program_only_in_sbin_is_found() {
        // The exact case that fails with Command::new: mkfs.erofs is installed into
        // /usr/sbin, which nothing on the fallback path covers.
        let only_sbin = |p: &Path| p == Path::new("/usr/sbin/mkfs.erofs");
        assert_eq!(
            resolve("mkfs.erofs", &only_sbin),
            Some(PathBuf::from("/usr/sbin/mkfs.erofs"))
        );
    }

    #[test]
    fn everything_is_resolved_before_anything_runs() {
        // Reporting a missing mkfs.erofs after an 80 MB download and an unpack into a
        // 5.5 GB /var is a poor way to find out the image is incomplete.
        let no_mkfs = |p: &Path| p.to_string_lossy().ends_with("tar");
        let error = Tools::find(&no_mkfs).unwrap_err();
        assert_eq!(error.program, "mkfs.erofs");
    }

    #[test]
    fn a_missing_program_is_named_an_image_fault() {
        let message = Missing {
            program: "mkfs.erofs".to_owned(),
        }
        .to_string();
        assert!(message.contains("image fault"), "{message}");
        assert!(
            message.contains("anything to do with the download"),
            "an incomplete image and a bad download need opposite responses, so the \
             message has to separate them: {message}"
        );
    }

    #[test]
    fn all_three_programs_resolve_when_they_are_present() {
        let everything = |_: &Path| true;
        let tools = Tools::find(&everything).unwrap();
        assert_eq!(tools.tar, PathBuf::from("/sbin/tar"));
        assert_eq!(tools.mkfs_erofs, PathBuf::from("/sbin/mkfs.erofs"));
        assert_eq!(tools.sha256sum, PathBuf::from("/sbin/sha256sum"));
    }
}
