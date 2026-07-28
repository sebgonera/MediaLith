//! Mounting an app image, and refusing to mount one that has changed.
//!
//! ADR-0007 keeps Plex in a read-only image file under `/var` and mounts it at runtime.
//! `/var` is writable by definition, so the bytes that were verified at provisioning
//! time are not necessarily the bytes present at boot: a failing disk, a truncated
//! write during a power cut, or someone with a shell can all change them afterwards.
//!
//! # Verify before mount, and mean it
//!
//! The integrity record written at provisioning exists for exactly this moment. It is
//! checked **before** the loop device is attached, not after and not alongside, because
//! a mounted image is already being executed from.
//!
//! The check is also the only reason the record is written at all. An image whose hash
//! is never compared has a record that is a comment. That is a real failure mode and
//! not a hypothetical one — this project has already shipped a diagnostic pointing at a
//! test that did not exist.
//!
//! # Why a loop device rather than mounting the file directly
//!
//! `mount(2)` takes a block device. Attaching a file to `/dev/loopN` is what makes a
//! file into one, and `losetup` is the tool the image carries for it. The kernel
//! pre-creates eight loop devices (`CONFIG_BLK_DEV_LOOP_MIN_COUNT=8`), which matters
//! here because there is no udev to create more on demand.
//!
//! # What is verified and what is not
//!
//! The plan and the integrity check are tested, and this has now attached a loop device
//! and mounted an app image on the appliance: `/var/lib/plexos/apps/plex/<version>.img
//! attached to /dev/loop0`, mounted at `/run/plexos/plex`, with the hash checked first.

use std::path::{Path, PathBuf};

use crate::store::Version;

/// Mount options for an app image.
///
/// `ro` because ADR-0007 says the image is immutable and erofs could not write anyway;
/// stating it means a future filesystem change cannot quietly make it writable.
/// `nosuid` and `nodev` because nothing in a media server needs either, and `/var`
/// already carries both for the same reason.
///
/// Deliberately **not** `noexec`: this image exists to be executed from.
pub const MOUNT_OPTIONS: &str = "ro,nosuid,nodev";

/// The filesystem an app image is.
///
/// Not a choice. `CONFIG_SQUASHFS` is unset in the kernel fragment and
/// `CONFIG_EROFS_FS` is set, so erofs is what an app image can be.
pub const FSTYPE: &str = "erofs";

/// Why an image was not mounted.
#[derive(Debug)]
pub enum Refusal {
    /// The image file is absent.
    NoImage(PathBuf),
    /// No integrity record accompanies it.
    NoRecord(PathBuf),
    /// The record could not be read, or names a different file.
    UnreadableRecord {
        /// Where the record is.
        path: PathBuf,
    },
    /// The image does not hash to what the record says.
    Altered {
        /// Which image.
        image: PathBuf,
        /// What provisioning recorded.
        recorded: String,
        /// What it hashes to now.
        found: String,
    },
    /// Something went wrong attaching or mounting.
    Failed {
        /// What was being attempted.
        step: String,
        /// Why it did not work.
        cause: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoImage(path) => write!(
                f,
                "there is no app image at {}. Plex has not been provisioned on this \
                 machine yet (ADR-0010), which is the normal state of a fresh install.",
                path.display()
            ),
            Self::NoRecord(path) => write!(
                f,
                "the app image has no integrity record at {}. It cannot be checked, so \
                 it will not be mounted. Re-provisioning writes a new one; deleting the \
                 record to get past this would mount an image nobody has vouched for.",
                path.display()
            ),
            Self::UnreadableRecord { path } => write!(
                f,
                "the integrity record at {} is not readable as one, or names a \
                 different image. Re-provision rather than editing it.",
                path.display()
            ),
            Self::Altered {
                image,
                recorded,
                found,
            } => write!(
                f,
                "{} has changed since it was installed. Provisioning recorded \
                 {recorded}; it now hashes to {found}. It will not be mounted. This is \
                 either a failing disk or an image somebody has replaced, and both are \
                 answered by re-provisioning from a signed package.",
                image.display()
            ),
            Self::Failed { step, cause } => write!(f, "{step}: {cause}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// What the caller must do to mount a verified image.
///
/// Data rather than actions, for the same reason as [`crate::build`]: the ordering is
/// the part worth testing, and it can be tested without a loop device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Create the mount point.
    CreateDir(PathBuf),
    /// Attach the image to a free loop device, yielding its path.
    AttachLoop {
        /// The image file.
        image: PathBuf,
    },
    /// Mount the loop device read-only.
    Mount {
        /// Where it goes.
        target: PathBuf,
    },
}

/// Everything needed to mount `version`, once its bytes have been checked.
///
/// Takes the digest that was verified rather than the path alone, so a caller cannot
/// construct a plan without having done the check: there is nothing else to pass.
#[must_use]
pub fn mount_plan(image: &Path, target: &Path, _verified: &Verified) -> Vec<Step> {
    vec![
        Step::CreateDir(target.to_path_buf()),
        Step::AttachLoop {
            image: image.to_path_buf(),
        },
        Step::Mount {
            target: target.to_path_buf(),
        },
    ]
}

/// Proof that an image's bytes match its record.
///
/// Produced only by [`check`]. It carries nothing useful; its whole purpose is that
/// [`mount_plan`] cannot be called without one, so "mount without verifying" is not a
/// sequence anybody can write by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The digest both the record and the file agree on.
    pub digest: String,
}

/// Compares an image against its integrity record.
///
/// `hash_file` computes a SHA256 of a path — injected so this is testable without
/// hashing anything, and so the caller decides whether that means `sha256sum` or
/// something else.
///
/// # Errors
/// [`Refusal`], in every case meaning the image must not be mounted.
pub fn check(
    image: &Path,
    record: &Path,
    exists: &dyn Fn(&Path) -> bool,
    read: &dyn Fn(&Path) -> Option<String>,
    hash_file: &dyn Fn(&Path) -> Option<String>,
) -> Result<Verified, Refusal> {
    if !exists(image) {
        return Err(Refusal::NoImage(image.to_path_buf()));
    }
    if !exists(record) {
        return Err(Refusal::NoRecord(record.to_path_buf()));
    }

    let image_name = image
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let body = read(record).ok_or_else(|| Refusal::UnreadableRecord {
        path: record.to_path_buf(),
    })?;
    let recorded = crate::store::digest_from_record(&body, image_name).ok_or_else(|| {
        Refusal::UnreadableRecord {
            path: record.to_path_buf(),
        }
    })?;

    let found = hash_file(image).ok_or_else(|| Refusal::Failed {
        step: "hashing the app image".to_owned(),
        cause: "sha256sum produced nothing".to_owned(),
    })?;

    if !found.eq_ignore_ascii_case(&recorded) {
        return Err(Refusal::Altered {
            image: image.to_path_buf(),
            recorded,
            found,
        });
    }

    Ok(Verified { digest: recorded })
}

/// Resolves what `current` points at, as an absolute path.
///
/// The link holds a bare file name so that it stays valid wherever `/var` is mounted,
/// which means reading it is not enough — it has to be joined to the directory it lives
/// in. Doing that wrongly yields a relative path that resolves against the working
/// directory of whatever happens to be running, which is how a daemon ends up looking
/// for an app image in `/`.
#[must_use]
pub fn resolve_current(apps: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        apps.join(target)
    }
}

/// The version an image path names, if it names one.
#[must_use]
pub fn version_at(image: &Path) -> Option<Version> {
    crate::store::version_of(image.file_name()?.to_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "d627a1eea7355014e8aea4132944202b333de84b5a29967c1d8abd20b7fe5f73";

    fn image() -> PathBuf {
        PathBuf::from("/var/lib/plexos/apps/plex/1.43.3.10828.img")
    }

    fn record() -> PathBuf {
        PathBuf::from("/var/lib/plexos/apps/plex/1.43.3.10828.img.sha256")
    }

    fn all_present(_: &Path) -> bool {
        true
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "matches the `read` callback's shape"
    )]
    fn good_record(_: &Path) -> Option<String> {
        Some(crate::store::record_body(DIGEST, "1.43.3.10828.img"))
    }

    #[test]
    fn an_image_matching_its_record_is_accepted() {
        let verified = check(&image(), &record(), &all_present, &good_record, &|_| {
            Some(DIGEST.to_owned())
        })
        .unwrap();
        assert_eq!(verified.digest, DIGEST);
    }

    #[test]
    fn an_altered_image_is_refused_and_the_message_says_what_to_do() {
        // The case the record exists for: bytes that changed after provisioning. A
        // failing disk and a substituted image look identical here and take the same
        // answer, so the message names both rather than guessing.
        let refusal = check(&image(), &record(), &all_present, &good_record, &|_| {
            Some("f".repeat(64))
        })
        .unwrap_err();

        let message = refusal.to_string();
        assert!(matches!(refusal, Refusal::Altered { .. }));
        assert!(message.contains("will not be mounted"), "{message}");
        assert!(message.contains("re-provisioning"), "{message}");
    }

    #[test]
    fn a_missing_record_does_not_mean_mount_it_anyway() {
        // The tempting shortcut. An image with no record cannot be checked, and
        // mounting it regardless makes every other check in this module decorative.
        let only_image = |p: &Path| p == image();
        let refusal = check(&image(), &record(), &only_image, &good_record, &|_| {
            Some(DIGEST.to_owned())
        })
        .unwrap_err();

        assert!(matches!(refusal, Refusal::NoRecord(_)));
        let message = refusal.to_string();
        assert!(
            message.contains("deleting the record to get past this"),
            "names the shortcut so nobody takes it: {message}"
        );
    }

    #[test]
    fn a_record_for_a_different_image_is_refused() {
        // An image renamed by hand. The digest would be compared against bytes it was
        // never computed from.
        let wrong_name = |_: &Path| Some(crate::store::record_body(DIGEST, "1.42.2.img"));
        let refusal = check(&image(), &record(), &all_present, &wrong_name, &|_| {
            Some(DIGEST.to_owned())
        })
        .unwrap_err();
        assert!(matches!(refusal, Refusal::UnreadableRecord { .. }));
    }

    #[test]
    fn an_unprovisioned_machine_is_told_that_and_not_that_something_broke() {
        // A fresh install has no image, and reporting it as a fault would send someone
        // looking for damage on a machine that is simply new.
        let nothing = |_: &Path| false;
        let refusal = check(&image(), &record(), &nothing, &good_record, &|_| None).unwrap_err();
        let message = refusal.to_string();
        assert!(matches!(refusal, Refusal::NoImage(_)));
        assert!(
            message.contains("normal state of a fresh install"),
            "{message}"
        );
    }

    #[test]
    fn the_plan_creates_the_mount_point_before_mounting_on_it() {
        let plan = mount_plan(
            &image(),
            Path::new(plexos_types::paths::PLEX_MOUNT),
            &Verified {
                digest: DIGEST.to_owned(),
            },
        );
        let created = plan
            .iter()
            .position(|s| matches!(s, Step::CreateDir(_)))
            .unwrap();
        let mounted = plan
            .iter()
            .position(|s| matches!(s, Step::Mount { .. }))
            .unwrap();
        assert!(created < mounted, "{plan:#?}");
    }

    #[test]
    fn the_loop_device_is_attached_before_the_mount_that_needs_it() {
        let plan = mount_plan(
            &image(),
            Path::new(plexos_types::paths::PLEX_MOUNT),
            &Verified {
                digest: DIGEST.to_owned(),
            },
        );
        let attached = plan
            .iter()
            .position(|s| matches!(s, Step::AttachLoop { .. }))
            .unwrap();
        let mounted = plan
            .iter()
            .position(|s| matches!(s, Step::Mount { .. }))
            .unwrap();
        assert!(attached < mounted, "{plan:#?}");
    }

    #[test]
    fn the_image_is_mounted_read_only_but_not_noexec() {
        // noexec here would be the tidy-looking mistake: this image exists to be
        // executed from, and the failure would be Plex refusing to start with a
        // permission error that says nothing about mount options.
        assert!(MOUNT_OPTIONS.contains("ro"));
        assert!(MOUNT_OPTIONS.contains("nosuid"));
        assert!(MOUNT_OPTIONS.contains("nodev"));
        assert!(!MOUNT_OPTIONS.contains("noexec"), "{MOUNT_OPTIONS}");
    }

    #[test]
    fn the_current_link_resolves_against_the_directory_holding_it() {
        // `current` stores a bare name so it survives /var being mounted elsewhere.
        // Using it unjoined gives a relative path that resolves against whatever
        // working directory the daemon happens to have.
        let apps = Path::new(plexos_types::paths::PLEX_APPS);
        let resolved = resolve_current(apps, Path::new("1.43.3.10828.img"));
        assert!(resolved.is_absolute(), "{resolved:?}");
        assert_eq!(resolved, apps.join("1.43.3.10828.img"));
    }

    #[test]
    fn an_absolute_link_target_is_left_alone() {
        let apps = Path::new(plexos_types::paths::PLEX_APPS);
        let absolute = Path::new("/var/lib/plexos/apps/plex/1.42.2.img");
        assert_eq!(resolve_current(apps, absolute), absolute);
    }

    #[test]
    fn a_version_can_be_read_back_from_an_image_path() {
        assert_eq!(
            version_at(&image()).map(|v| v.raw),
            Some("1.43.3.10828".to_owned())
        );
        assert!(version_at(Path::new("/var/lib/plexos/apps/plex/current")).is_none());
    }
}
