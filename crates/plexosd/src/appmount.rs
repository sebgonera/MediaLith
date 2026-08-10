//! Mounting the provisioned Plex image at boot, and saying so when there is none.
//!
//! The verification and the plan live in `plexos_plex::mount`, which knows nothing
//! about this machine. This is the part that points them at the real paths, runs them,
//! and turns the outcome into something a person reads on a console.
//!
//! # Not provisioned is not a failure
//!
//! Every appliance boots at least once with no Plex on it: ADR-0010 fetches it at first
//! boot and it may never have happened. That state is reported as information, not as
//! an error, and the boot continues. Treating it as a fault would mean a fresh install
//! looked broken, which is precisely the machine whose owner is least able to tell.
//!
//! # What is verified and what is not
//!
//! **This has never run on the appliance.** The paths, the ordering and the refusals
//! are covered by tests in `plexos_plex::mount`; attaching a loop device and mounting
//! erofs are two syscalls that no test here performs. Delete this notice once a
//! provisioned machine has booted with Plex mounted.

use std::path::{Path, PathBuf};

use plexos_plex::{mount, tools};
use plexos_types::paths;

/// What happened when the app image was mounted.
#[derive(Debug)]
pub enum Outcome {
    /// Mounted, and where the loop device ended up.
    Mounted {
        /// The version now mounted.
        version: String,
        /// The loop device backing it, for `losetup -a` to make sense of.
        device: PathBuf,
    },
    /// No Plex on this machine yet. The normal state of a fresh install.
    NotProvisioned,
    /// There is an image and it was not mounted.
    Refused(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mounted { version, device } => write!(
                f,
                "Plex {version} mounted at {} from {}",
                paths::PLEX_MOUNT,
                device.display()
            ),
            Self::NotProvisioned => write!(
                f,
                "no Plex installed yet — provision it from the console at \
                 http://<address>/ (ADR-0010). The system is otherwise fine."
            ),
            Self::Refused(why) => write!(f, "Plex was not mounted: {why}"),
        }
    }
}

/// Reads `current`, verifies what it points at, and mounts it.
///
/// Takes the apps directory rather than assuming it, so the whole thing can be pointed
/// at a scratch directory during development without touching `/var`.
pub fn mount_current(apps: &Path, target: &Path, log: &mut dyn FnMut(&str)) -> Outcome {
    let link = apps.join(plexos_plex::store::CURRENT_LINK);
    let Ok(pointed_at) = std::fs::read_link(&link) else {
        return Outcome::NotProvisioned;
    };

    // The link holds a bare name; joining it to the directory is what makes it
    // absolute. See mount::resolve_current for why doing this wrongly is subtle.
    let image = mount::resolve_current(apps, &pointed_at);
    let Some(version) = mount::version_at(&image) else {
        return Outcome::Refused(format!(
            "`current` points at {}, which is not a version-named app image. Something \
             other than provisioning wrote this link.",
            pointed_at.display()
        ));
    };

    let record = image.with_file_name(version.record_name());
    let tools = match tools::MountTools::on_this_system() {
        Ok(tools) => tools,
        Err(missing) => return Outcome::Refused(missing.to_string()),
    };

    let verified = match mount::check(
        &image,
        &record,
        &|p: &Path| p.exists(),
        &|p: &Path| std::fs::read_to_string(p).ok(),
        &|p: &Path| sha256(&tools, p),
    ) {
        Ok(verified) => verified,
        // A dangling `current` and an unprovisioned machine look the same to a person
        // and are told apart here: the link existed, so something did install Plex once.
        Err(mount::Refusal::NoImage(path)) => {
            return Outcome::Refused(format!(
                "`current` points at {}, which does not exist. An install was \
                 interrupted, or retention removed an image that was still active.",
                path.display()
            ));
        }
        Err(refusal) => return Outcome::Refused(refusal.to_string()),
    };

    let plan = mount::mount_plan(&image, target, &verified);
    match plexos_plex::execute::mount_plan(&plan, &tools, log) {
        Ok(device) => Outcome::Mounted {
            version: version.raw,
            device,
        },
        Err(failure) => Outcome::Refused(failure.to_string()),
    }
}

/// Where the kernel lists what is mounted.
const PROC_MOUNTS: &str = "/proc/mounts";

/// What happened when the app image was taken down.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// Nothing was mounted there, which is not a failure.
    NotMounted,
    /// Unmounted, and the loop device that backed it detached.
    Removed {
        /// The device that was released, if it was a loop device.
        device: Option<String>,
    },
    /// It is still mounted, and this says why.
    Failed(String),
}

impl std::fmt::Display for Removal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotMounted => write!(f, "no app image was mounted"),
            Self::Removed { device: Some(d) } => {
                write!(f, "the app image was unmounted and {d} released")
            }
            Self::Removed { device: None } => write!(f, "the app image was unmounted"),
            Self::Failed(why) => write!(
                f,
                "the app image could not be unmounted: {why}. Remedy: something still has \
                 a file open on it -- Plex itself, most likely, which has to be stopped \
                 first. The version that was running is still running, which is the safe \
                 half of this failing."
            ),
        }
    }
}

/// The device mounted at `target`, according to the kernel's own list.
///
/// Read rather than remembered. `plexosd` can be restarted while an image stays mounted,
/// so a device recorded in this process is a device this process may never have attached —
/// and detaching the wrong loop device would pull the floor out from under something else.
///
/// Fields are separated by single spaces and paths escape space, tab, newline and
/// backslash as octal. Only the space matters here in practice, and all four are handled
/// because handling three of them is the kind of thing that works until a media directory
/// is named with a tab in it.
#[must_use]
pub fn device_at(mounts: &str, target: &Path) -> Option<String> {
    mounts.lines().find_map(|line| {
        let mut fields = line.split(' ');
        let device = fields.next()?;
        let mounted_on = unescape(fields.next()?);
        (Path::new(&mounted_on) == target).then(|| unescape(device))
    })
}

/// Decodes the octal escapes the kernel writes into `/proc/mounts`.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut bytes = field.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(char::from(byte));
            continue;
        }
        let digits: String = bytes.by_ref().take(3).map(char::from).collect();
        if let Ok(decoded) = u8::from_str_radix(&digits, 8) {
            out.push(char::from(decoded));
        } else {
            // Not an escape after all. Put back what was read rather than dropping it: a
            // path this cannot decode must not silently become a different path.
            out.push('\\');
            out.push_str(&digits);
        }
    }
    out
}

/// Unmounts the app image and releases the loop device behind it.
///
/// Plex has to be stopped first. This does not stop it — the caller owns that decision,
/// and an unmount that killed the running server to get its way would be a worse thing
/// than a failed unmount.
pub fn unmount_current(target: &Path, log: &mut dyn FnMut(&str)) -> Removal {
    let mounts = std::fs::read_to_string(PROC_MOUNTS).unwrap_or_default();
    let Some(device) = device_at(&mounts, target) else {
        return Removal::NotMounted;
    };

    if let Err(error) = plexos_sys::mount::unmount(&target.to_string_lossy()) {
        return Removal::Failed(error.to_string());
    }
    log(&format!("{} unmounted", target.display()));

    // Only a loop device is detached, and only after the unmount succeeded. The kernel
    // pre-creates eight of them, so leaking one per swap gives an appliance that stops
    // being able to mount an app image after the eighth Plex update -- with an error about
    // loop devices, months later, on a machine whose owner changed nothing.
    if !device.starts_with("/dev/loop") {
        return Removal::Removed { device: None };
    }

    let Ok(tools) = tools::MountTools::on_this_system() else {
        return Removal::Removed { device: None };
    };
    match std::process::Command::new(&tools.losetup)
        .arg("-d")
        .arg(&device)
        .output()
    {
        Ok(output) if output.status.success() => Removal::Removed {
            device: Some(device),
        },
        Ok(output) => {
            // Not a failure of the unmount, which has already happened. Reported and
            // survived: the image is down, and one leaked loop device out of eight is a
            // thing to notice rather than a thing to stop for.
            log(&format!(
                "{device} could not be detached ({}), so one of the eight loop devices \\
                 stays in use until the next reboot",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
            Removal::Removed { device: None }
        }
        Err(error) => {
            log(&format!("{device} could not be detached ({error})"));
            Removal::Removed { device: None }
        }
    }
}

/// SHA256 of a file, via the tool the image carries.
fn sha256(tools: &tools::MountTools, path: &Path) -> Option<String> {
    let output = std::process::Command::new(&tools.sha256sum)
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

/// Reads the app image store off the disk.
///
/// [`plexos_plex::store`] decides what the store *means* -- which version is current,
/// which are superseded -- and deliberately opens no files, so that every retention rule
/// is testable without one. This is the other half: the listing and the symlink target it
/// needs. Keeping the split means a wrong answer here is a wrong reading of a directory,
/// never a wrong policy.
///
/// A directory that is not there yet is an empty store rather than an error: that is every
/// appliance until Plex is installed, and the console asks this on every poll.
#[must_use]
pub fn read_store(apps: &Path) -> plexos_plex::store::Store {
    let entries: Vec<String> = std::fs::read_dir(apps)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    // read_link rather than canonicalize: the link is relative and its target may have been
    // removed by hand, and a store that describes a dangling `current` is more useful than
    // one that refuses to describe anything.
    let target = std::fs::read_link(apps.join(plexos_plex::store::CURRENT_LINK))
        .ok()
        .map(|path| {
            path.file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
        });

    plexos_plex::store::Store::from_listing(&entries, target.as_deref())
}

/// Deletes the images ADR-0007's retention policy no longer keeps, and their records.
///
/// Called after a successful install, which is the only moment the answer changes. Returns
/// what it removed, for the log.
///
/// Failures are collected rather than propagated: a full `/var` or a file held open must
/// not turn a successful Plex install into a failed one. The cost of leaving an old image
/// behind is disk space; the cost of failing here would be a machine that has the new Plex
/// running and reports that installing it did not work.
pub fn prune_superseded(apps: &Path, log: &mut dyn FnMut(&str)) -> Vec<String> {
    let store = read_store(apps);
    let Some(current) = store.current.clone() else {
        // Nothing to be superseded *by*. Removing images while `current` names none would
        // be deleting on the strength of a reading that already failed.
        return Vec::new();
    };

    let mut removed = Vec::new();
    for version in store.superseded(&current) {
        let image = apps.join(version.image_name());
        let record = apps.join(version.record_name());
        match std::fs::remove_file(&image) {
            Ok(()) => {
                let _ = std::fs::remove_file(&record);
                log(&format!("removed superseded Plex {}", version.raw));
                removed.push(version.raw.clone());
            }
            Err(error) => log(&format!(
                "could not remove superseded Plex {}: {error}. \
                 Remedy: it is only disk space, and the next install will try again.",
                version.raw
            )),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plexos-appmount-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Lays out an apps directory: images, and what `current` points at.
    fn apps_with(dir: &Path, images: &[&str], current: Option<&str>) {
        for image in images {
            std::fs::write(dir.join(image), b"not really an image").unwrap();
            std::fs::write(dir.join(format!("{image}.sha256")), b"digest  name\n").unwrap();
        }
        if let Some(target) = current {
            std::os::unix::fs::symlink(target, dir.join("current")).unwrap();
        }
    }

    #[test]
    fn the_store_is_read_from_the_directory_and_the_symlink() {
        let dir = scratch("read-store");
        apps_with(
            &dir,
            &["1.42.2.10156.img", "1.43.3.10828.img"],
            Some("1.43.3.10828.img"),
        );

        let store = read_store(&dir);
        assert_eq!(store.installed.len(), 2);
        assert_eq!(
            store.current.map(|v| v.raw),
            Some("1.43.3.10828".to_owned()),
            "the current version comes from the link, not from the newest file"
        );
    }

    #[test]
    fn a_directory_that_does_not_exist_is_an_empty_store() {
        // Every appliance until Plex is installed, and the console asks on every poll.
        let store = read_store(&scratch("absent").join("never-created"));
        assert!(store.installed.is_empty());
        assert!(store.current.is_none());
    }

    #[test]
    fn a_dangling_current_still_describes_what_is_on_disk() {
        // An image removed by hand. Refusing to describe the store because the link is
        // broken would hide the very images somebody would want to point it back at.
        // Named for this test, not for what it is about: `scratch("dangling")` was already
        // taken by a_dangling_current_link_is_distinguished_from_never_having_installed,
        // and `scratch` deletes the directory before creating it -- so the two raced,
        // each removing what the other had just written, and failed in whichever order the
        // scheduler picked. Caught here while hunting a different flake entirely.
        let dir = scratch("dangling-still-describes-disk");
        apps_with(&dir, &["1.42.2.10156.img"], Some("1.99.9.9999.img"));

        let store = read_store(&dir);
        assert_eq!(store.installed.len(), 1, "the real image is still listed");
    }

    #[test]
    fn pruning_keeps_the_current_image_and_one_previous() {
        // ADR-0007's retention, exercised against a disk rather than a list. Three
        // versions in, one comes out, and it is the oldest of the two that are not current.
        let dir = scratch("prune-keeps-two");
        apps_with(
            &dir,
            &["1.41.1.1000.img", "1.42.2.10156.img", "1.43.3.10828.img"],
            Some("1.43.3.10828.img"),
        );

        let removed = prune_superseded(&dir, &mut |_| {});
        assert_eq!(removed, vec!["1.41.1.1000".to_owned()]);
        assert!(
            dir.join("1.43.3.10828.img").exists(),
            "the running one stays"
        );
        assert!(dir.join("1.42.2.10156.img").exists(), "the way back stays");
        assert!(!dir.join("1.41.1.1000.img").exists());
        assert!(
            !dir.join("1.41.1.1000.img.sha256").exists(),
            "the integrity record goes with its image, or it describes nothing"
        );
    }

    #[test]
    fn pruning_removes_nothing_when_current_names_nothing() {
        // Deleting on the strength of a reading that already failed is how a retention
        // policy takes down the version that works.
        let dir = scratch("prune-no-current");
        apps_with(&dir, &["1.41.1.1000.img", "1.42.2.10156.img"], None);

        assert!(prune_superseded(&dir, &mut |_| {}).is_empty());
        assert!(dir.join("1.41.1.1000.img").exists());
    }

    #[test]
    fn a_machine_with_no_current_link_is_unprovisioned_rather_than_broken() {
        // The state of every appliance before ADR-0010's first-boot flow has run. A
        // fresh install must not look like a fault, because its owner is the person
        // least able to tell the difference.
        let apps = scratch("fresh");
        let outcome = mount_current(&apps, Path::new("/nonexistent"), &mut |_| {});
        assert!(matches!(outcome, Outcome::NotProvisioned));
        assert!(outcome.to_string().contains("otherwise fine"), "{outcome}");
        let _ = std::fs::remove_dir_all(&apps);
    }

    #[test]
    fn a_dangling_current_link_is_distinguished_from_never_having_installed() {
        // These look identical to a person and need different answers: one says
        // "install Plex", the other says "something removed the image you were using".
        let apps = scratch("dangling");
        std::os::unix::fs::symlink("1.43.3.10828.img", apps.join("current")).unwrap();

        let outcome = mount_current(&apps, Path::new("/nonexistent"), &mut |_| {});
        let message = outcome.to_string();
        assert!(matches!(outcome, Outcome::Refused(_)), "{message}");
        assert!(message.contains("does not exist"), "{message}");
        assert!(
            message.contains("interrupted") || message.contains("retention"),
            "names what could have caused it: {message}"
        );
        let _ = std::fs::remove_dir_all(&apps);
    }

    #[test]
    fn a_current_link_pointing_at_something_that_is_not_an_image_is_refused() {
        let apps = scratch("nonsense");
        std::fs::write(apps.join("notes.txt"), "hello").unwrap();
        std::os::unix::fs::symlink("notes.txt", apps.join("current")).unwrap();

        let outcome = mount_current(&apps, Path::new("/nonexistent"), &mut |_| {});
        let message = outcome.to_string();
        assert!(
            message.contains("not a version-named app image"),
            "{message}"
        );
        let _ = std::fs::remove_dir_all(&apps);
    }

    #[test]
    fn an_image_without_a_record_is_refused_before_anything_is_attached() {
        // The security property, at the level a person sees it: no record, no mount,
        // and the reason says so rather than mentioning loop devices.
        let apps = scratch("norecord");
        std::fs::write(apps.join("1.43.3.10828.img"), b"not really an image").unwrap();
        std::os::unix::fs::symlink("1.43.3.10828.img", apps.join("current")).unwrap();

        let outcome = mount_current(&apps, Path::new("/nonexistent"), &mut |_| {});
        let message = outcome.to_string();
        assert!(matches!(outcome, Outcome::Refused(_)), "{message}");
        assert!(message.contains("integrity record"), "{message}");
        let _ = std::fs::remove_dir_all(&apps);
    }

    #[test]
    fn an_image_that_does_not_match_its_record_is_refused() {
        // The whole reason the record is written. The bytes here hash to something
        // other than the digest recorded beside them.
        let apps = scratch("altered");
        std::fs::write(apps.join("1.43.3.10828.img"), b"tampered").unwrap();
        std::fs::write(
            apps.join("1.43.3.10828.img.sha256"),
            plexos_plex::store::record_body(&"a".repeat(64), "1.43.3.10828.img"),
        )
        .unwrap();
        std::os::unix::fs::symlink("1.43.3.10828.img", apps.join("current")).unwrap();

        let outcome = mount_current(&apps, Path::new("/nonexistent"), &mut |_| {});
        let message = outcome.to_string();
        assert!(
            message.contains("has changed since it was installed"),
            "{message}"
        );
        assert!(message.contains("will not be mounted"), "{message}");
        let _ = std::fs::remove_dir_all(&apps);
    }

    /// Lines copied from a real `/proc/mounts`, not composed here.
    ///
    /// The rule this project keeps relearning: a fixture you imagined is a test that agrees
    /// with your code and not with the machine. `resolv.conf` was parsed with guessed
    /// comment rules and the parser was wrong on the appliance while its test passed.
    const REAL_MOUNTS: &str = "\
/dev/loop0 /snap/core24/1587 squashfs ro,nodev,relatime,errors=continue,threads=single 0 0
/dev/loop3 /snap/desktop-security-center/151 squashfs ro,nodev,relatime,errors=continue,threads=single 0 0
/dev/mapper/ubuntu--vg-ubuntu--lv / ext4 rw,relatime 0 0
/dev/sda2 /boot ext4 rw,relatime 0 0
";

    #[test]
    fn the_device_behind_a_mount_point_is_read_from_the_kernels_own_list() {
        assert_eq!(
            device_at(REAL_MOUNTS, Path::new("/snap/core24/1587")).as_deref(),
            Some("/dev/loop0")
        );
        assert_eq!(
            device_at(REAL_MOUNTS, Path::new("/")).as_deref(),
            Some("/dev/mapper/ubuntu--vg-ubuntu--lv")
        );
        assert_eq!(device_at(REAL_MOUNTS, Path::new("/run/plexos/plex")), None);
    }

    #[test]
    fn a_mount_point_with_a_space_in_it_is_matched_rather_than_truncated() {
        // The kernel escapes it as \040. A parser that split on whitespace would see the
        // mount point as "/var/media/My" and never match -- and the consequence here is
        // not a bad message, it is detaching the wrong loop device.
        let mounts = "/dev/loop4 /var/media/My\\040Films erofs ro 0 0\n";
        assert_eq!(
            device_at(mounts, Path::new("/var/media/My Films")).as_deref(),
            Some("/dev/loop4")
        );
    }

    #[test]
    fn something_that_is_not_an_escape_survives_unchanged() {
        // A path must never silently become a different path. `\0` here is not a valid
        // three-digit octal escape and has to come back out as it went in.
        assert_eq!(unescape("/a\\0b"), "/a\\0b");
        assert_eq!(unescape("/plain/path"), "/plain/path");
        assert_eq!(unescape("/tab\\011here"), "/tab\there");
    }

    #[test]
    fn unmounting_nothing_is_not_a_failure() {
        // Every appliance is in this state until Plex is installed, and a swap on one that
        // was never mounted must proceed to the mount rather than stopping.
        let outcome = unmount_current(&scratch("nothing").join("nowhere"), &mut |_| {});
        assert_eq!(outcome, Removal::NotMounted);
    }

    #[test]
    fn a_failed_unmount_says_what_is_holding_it_and_that_plex_still_runs() {
        // The likely cause by far, and the half of it worth saying: the version that was
        // running is still running. A message that only said "busy" would read as a
        // machine that had lost its Plex.
        let failed = Removal::Failed("Device or resource busy".to_owned());
        let message = failed.to_string();
        assert!(message.contains("Remedy:"), "{message}");
        assert!(message.contains("stopped first"), "{message}");
        assert!(message.contains("still running"), "{message}");
    }
}
