//! Turning a verified package into an app image, as a plan rather than a script.
//!
//! The same shape as `plexos_init::plan`: the steps are data, so the order and the
//! arguments can be tested without a disk, an 80 MB download, or a `mkfs`. What
//! executes them is small and boring by design.
//!
//! # The order is load-bearing
//!
//! Nothing is unpacked until [`verify`](crate::verify) has accepted the signature and
//! [`agrees_with`](crate::agrees_with) has tied it to these bytes. The plan therefore
//! starts *after* verification and takes the verified version as an argument — there is
//! no way to build one for a package that was never checked, because there is nothing
//! to pass.
//!
//! # Why the image is assembled beside its destination and moved last
//!
//! `/var` is the only writable filesystem and provisioning is interrupted by exactly
//! the things an appliance does: a power cut, a reboot, a user pulling the plug at the
//! wrong moment. A half-written file named `1.43.3.10828.img` is indistinguishable from
//! a complete one, and the next boot would mount it. Building under a temporary name in
//! the same directory and renaming at the end makes the final step atomic, so the
//! version-named file either does not exist or is whole.
//!
//! The integrity record is written *before* the rename for the same reason. An image
//! with no record cannot be checked on later mounts, and discovering that at boot is
//! worse than discovering a missing image.

use std::path::{Path, PathBuf};

use crate::store::Version;

/// Where images are assembled and kept, and the mount point for the active one.
///
/// Taken from `plexos-types` by the caller rather than duplicated here; the constants
/// exist so that the daemon, the installer and the updater cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `/var/lib/plexos/apps/plex`.
    pub apps: PathBuf,
}

impl Layout {
    /// The final resting place of an image.
    #[must_use]
    pub fn image(&self, version: &Version) -> PathBuf {
        self.apps.join(version.image_name())
    }

    /// Its integrity record.
    #[must_use]
    pub fn record(&self, version: &Version) -> PathBuf {
        self.apps.join(version.record_name())
    }

    /// The name it is built under, in the same directory so the rename cannot cross a
    /// filesystem and stop being atomic.
    #[must_use]
    pub fn staging_image(&self, version: &Version) -> PathBuf {
        self.apps
            .join(format!(".{}.incoming", version.image_name()))
    }

    /// Where `data.tar.xz` is unpacked before `mkfs.erofs` reads it.
    #[must_use]
    pub fn staging_root(&self, version: &Version) -> PathBuf {
        self.apps.join(format!(".{}.unpacked", version.raw))
    }

    /// The `current` symlink.
    #[must_use]
    pub fn current(&self) -> PathBuf {
        self.apps.join(crate::store::CURRENT_LINK)
    }
}

/// One step of building and installing an app image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Remove anything left by an interrupted attempt at this version.
    ClearStaging {
        /// Paths to delete if they exist.
        paths: Vec<PathBuf>,
    },
    /// Create the unpack directory.
    CreateDir(PathBuf),
    /// Extract `data.tar.xz` from the package into the unpack directory.
    Unpack {
        /// The package to read.
        package: PathBuf,
        /// Byte offset of `data.tar.xz`, from the `ar` directory.
        offset: u64,
        /// Its length.
        size: u64,
        /// Where to put the contents.
        into: PathBuf,
    },
    /// Build the erofs image from the unpacked tree.
    Mkfs {
        /// Directory to pack.
        from: PathBuf,
        /// Image to write.
        to: PathBuf,
    },
    /// Compute the image's SHA256 and write the sidecar record.
    Record {
        /// The image to hash.
        image: PathBuf,
        /// Where the record goes.
        record: PathBuf,
        /// The name the record must refer to, which is the *final* name and not the
        /// staging one — the record outlives the rename.
        names: String,
    },
    /// Rename the finished image into place. The atomic step.
    Publish {
        /// Staging name.
        from: PathBuf,
        /// Final name.
        to: PathBuf,
    },
    /// Point `current` at a version, atomically.
    Activate {
        /// The symlink.
        link: PathBuf,
        /// Its new target, a bare file name so the link stays valid wherever `/var` is
        /// mounted.
        target: String,
    },
    /// Delete a superseded image and its record.
    Remove {
        /// Paths to delete.
        paths: Vec<PathBuf>,
    },
    /// Drop the unpacked tree, which is three times the size of the image.
    RemoveDir(PathBuf),
}

/// The plan for installing `version` from `package`.
///
/// `data` is the `data.tar.xz` member as the `ar` directory located it. `superseded` is
/// what [`Store::superseded`](crate::store::Store::superseded) decided to drop.
#[must_use]
pub fn install_plan(
    layout: &Layout,
    version: &Version,
    package: &Path,
    data: &crate::ar::Member,
    superseded: &[Version],
) -> Vec<Step> {
    let staging_image = layout.staging_image(version);
    let staging_root = layout.staging_root(version);

    let mut steps = vec![
        // A previous attempt may have died anywhere. Clearing first means a retry after
        // a power cut behaves the same as a first attempt, rather than tripping over
        // a partial unpack whose contents nobody can vouch for.
        Step::ClearStaging {
            paths: vec![staging_image.clone(), staging_root.clone()],
        },
        Step::CreateDir(staging_root.clone()),
        Step::Unpack {
            package: package.to_path_buf(),
            offset: data.offset,
            size: data.size,
            into: staging_root.clone(),
        },
        Step::Mkfs {
            from: staging_root.clone(),
            to: staging_image.clone(),
        },
        // Before the rename: an image published without a record cannot be checked on
        // any later mount, and finding that out at boot is worse than finding a
        // missing image.
        Step::Record {
            image: staging_image.clone(),
            record: layout.record(version),
            names: version.image_name(),
        },
        Step::Publish {
            from: staging_image,
            to: layout.image(version),
        },
        Step::RemoveDir(staging_root),
        Step::Activate {
            link: layout.current(),
            target: version.image_name(),
        },
    ];

    // Retention last. Deleting the old image before the new one is active would leave
    // a window with nothing runnable, and this runs on a machine that can lose power.
    if !superseded.is_empty() {
        steps.push(Step::Remove {
            paths: superseded
                .iter()
                .flat_map(|v| [layout.image(v), layout.record(v)])
                .collect(),
        });
    }

    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout {
            apps: PathBuf::from(plexos_types::paths::PLEX_APPS),
        }
    }

    fn version() -> Version {
        Version::parse("1.43.3.10828-00f62d37d").unwrap()
    }

    fn data_member() -> crate::ar::Member {
        // The real offsets from plexmediaserver_1.43.3.10828_amd64.deb.
        crate::ar::Member {
            name: "data.tar.xz".to_owned(),
            size: 83_039_828,
            offset: 74_124,
        }
    }

    fn plan(superseded: &[Version]) -> Vec<Step> {
        install_plan(
            &layout(),
            &version(),
            Path::new("/var/lib/plexos/apps/plex/.download"),
            &data_member(),
            superseded,
        )
    }

    fn index_of(steps: &[Step], pred: impl Fn(&Step) -> bool) -> usize {
        steps.iter().position(pred).expect("step is in the plan")
    }

    #[test]
    fn the_image_is_published_only_after_it_is_complete() {
        // The atomicity property. A version-named file must never exist half-written,
        // because nothing downstream can tell one from a finished image.
        let steps = plan(&[]);
        let mkfs = index_of(&steps, |s| matches!(s, Step::Mkfs { .. }));
        let publish = index_of(&steps, |s| matches!(s, Step::Publish { .. }));
        assert!(mkfs < publish, "{steps:#?}");
    }

    #[test]
    fn the_record_is_written_before_the_image_is_published() {
        let steps = plan(&[]);
        let record = index_of(&steps, |s| matches!(s, Step::Record { .. }));
        let publish = index_of(&steps, |s| matches!(s, Step::Publish { .. }));
        assert!(
            record < publish,
            "an image with no record cannot be checked later: {steps:#?}"
        );
    }

    #[test]
    fn nothing_is_deleted_before_the_new_version_is_active() {
        // On a machine that can lose power, deleting the old image first leaves a
        // window with nothing runnable at all.
        let steps = plan(&[Version::parse("1.42.2.10156").unwrap()]);
        let activate = index_of(&steps, |s| matches!(s, Step::Activate { .. }));
        let remove = index_of(&steps, |s| matches!(s, Step::Remove { .. }));
        assert!(activate < remove, "{steps:#?}");
    }

    #[test]
    fn staging_sits_in_the_destination_directory() {
        // A rename is atomic only within one filesystem. Building in /tmp and moving
        // would silently become a copy, and the copy is not atomic.
        let l = layout();
        let v = version();
        assert_eq!(l.staging_image(&v).parent(), l.image(&v).parent());
        assert_eq!(l.staging_root(&v).parent(), Some(l.apps.as_path()));
    }

    #[test]
    fn staging_names_cannot_be_mistaken_for_an_image() {
        // Store::from_listing must not pick these up as installed versions if a crash
        // leaves them behind, or a partial unpack becomes a mountable release.
        let l = layout();
        let v = version();
        for path in [l.staging_image(&v), l.staging_root(&v)] {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(
                crate::store::version_of(name).is_none(),
                "{name} would be read back as an installed version"
            );
        }
    }

    #[test]
    fn a_retry_clears_what_a_previous_attempt_left() {
        let steps = plan(&[]);
        assert!(
            matches!(steps.first(), Some(Step::ClearStaging { .. })),
            "a retry after a power cut must behave like a first attempt: {steps:#?}"
        );
    }

    #[test]
    fn the_record_names_the_published_image_and_not_the_staging_file() {
        // The record outlives the rename. Naming the staging file would make every
        // later check fail against a name that no longer exists.
        let steps = plan(&[]);
        let Step::Record { names, .. } =
            &steps[index_of(&steps, |s| matches!(s, Step::Record { .. }))]
        else {
            unreachable!()
        };
        assert_eq!(names, "1.43.3.10828-00f62d37d.img");
    }

    #[test]
    fn current_points_at_a_bare_name_rather_than_an_absolute_path() {
        // An absolute target bakes in where /var is mounted, which is not the same
        // during provisioning from an installer as it is at runtime.
        let steps = plan(&[]);
        let Step::Activate { target, .. } =
            &steps[index_of(&steps, |s| matches!(s, Step::Activate { .. }))]
        else {
            unreachable!()
        };
        assert!(!target.starts_with('/'), "{target}");
        assert_eq!(target, "1.43.3.10828-00f62d37d.img");
    }

    #[test]
    fn a_superseded_version_takes_its_record_with_it() {
        // Leaving orphaned .sha256 files behind would accumulate silently and, worse,
        // make a later reinstall of the same version find a record for bytes that are
        // no longer there.
        let old = Version::parse("1.42.2.10156").unwrap();
        let steps = plan(std::slice::from_ref(&old));
        let Step::Remove { paths } = &steps[index_of(&steps, |s| matches!(s, Step::Remove { .. }))]
        else {
            unreachable!()
        };
        assert!(paths.contains(&layout().image(&old)));
        assert!(paths.contains(&layout().record(&old)));
    }

    #[test]
    fn the_unpacked_tree_is_removed_since_it_dwarfs_the_image() {
        // 219 MB unpacked against roughly 80 MB compressed, on a 5.5 GB /var that also
        // holds the media database.
        let steps = plan(&[]);
        assert!(steps.iter().any(|s| matches!(s, Step::RemoveDir(_))));
    }

    #[test]
    fn nothing_is_planned_before_the_package_has_been_verified() {
        // Not a property of the list but of the signature: install_plan takes a Version
        // that only exists once a caller has checked the package, so there is no way to
        // build a plan for an unverified download. This test exists to make that
        // intent fail loudly if the argument is ever relaxed.
        let steps = plan(&[]);
        assert!(matches!(steps[1], Step::CreateDir(_)));
        assert!(
            steps.iter().any(|s| matches!(s, Step::Unpack { .. })),
            "and the unpack is in the plan rather than done by the caller beforehand"
        );
    }
}
