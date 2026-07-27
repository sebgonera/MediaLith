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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plexos-appmount-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
}
