//! Clearing the boot try counter on the ESP.
//!
//! Marking a boot good is a rename, and nothing else (ADR-0005). The ESP is not
//! mounted during normal operation — it is needed twice in the life of a boot, here
//! and when an update stages a new UKI — so this mounts it, renames, and unmounts.
//!
//! Leaving a FAT filesystem mounted read-write on an appliance that may lose power at
//! any moment is a good way to corrupt the one partition the machine cannot boot
//! without.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use plexos_types::partition::LABEL_ESP;

use crate::bootcounter::BootEntry;

/// Where boot entries live on the ESP, per ADR-0005 and `post-image.sh`.
pub const ENTRY_DIR: &str = "EFI/Linux";

/// Where the ESP is mounted while the counter is cleared.
pub const MOUNT_POINT: &str = "/run/plexos/esp";

/// Boot entries found on a mounted ESP, newest-looking first is not implied — the
/// order is whatever the filesystem returned, sorted for determinism.
///
/// # Errors
///
/// If the entry directory cannot be read.
pub fn entries(esp: &Path) -> io::Result<Vec<(PathBuf, BootEntry)>> {
    let dir = esp.join(ENTRY_DIR);
    let mut found = Vec::new();

    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(parsed) = BootEntry::parse(name) {
            found.push((path, parsed));
        }
    }

    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Renames an entry to its counter-free form, making the slot permanent.
///
/// Idempotent: an entry with no counter is already good.
///
/// # Errors
///
/// If the rename fails, which leaves the counter standing — the safe direction. The
/// boot is not marked good, and a system that keeps failing to mark itself good will
/// eventually roll back, which is a visible symptom rather than a silent one.
pub fn mark_good(path: &Path, entry: &BootEntry) -> io::Result<Option<PathBuf>> {
    if !entry.is_on_trial() {
        return Ok(None);
    }

    if entry.is_exhausted() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to mark {entry} good: its counter is exhausted, so the \
                 bootloader has already given up on it. Marking it now would \
                 resurrect an image that failed to boot three times."
            ),
        ));
    }

    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        ));
    };

    let target = parent.join(entry.marked_good());
    fs::rename(path, &target)?;
    Ok(Some(target))
}

/// Installs a boot entry for a freshly written slot, on trial.
///
/// The name carries the try counter, which is the whole of ADR-0005: `+3` means three
/// attempts remain and none has been used. `systemd-boot` decrements it by renaming
/// before handing off, and the health gate drops the suffix once the boot proves itself.
///
/// # What this deliberately does not do
///
/// **It does not remove the entry that is currently good.** That entry is the rollback
/// target: if the new slot fails three times its counter reaches zero, `systemd-boot`
/// sorts it last, and the old entry is chosen again. Tidying it away would turn a
/// recoverable bad update into an unbootable machine, which is the one failure this
/// whole mechanism exists to prevent.
///
/// The new entry wins because it sorts higher, not because the old one was deleted.
/// `plexos_update::plan` refuses a bundle whose version does not sort above the running
/// one for exactly that reason.
///
/// It does not remove the wreckage of a *failed* update either, which is a different
/// thing and does need removing — see [`remove_wreckage`], which the update path calls
/// alongside this one so that what it cleared can be reported.
///
/// # Errors
/// If the entry directory cannot be created or the file cannot be written. A partial
/// write is left behind rather than cleaned up: it carries a `+3` counter and a version
/// that has never booted, so the bootloader will try it, fail, and fall back — whereas a
/// cleanup path that itself failed halfway is a state nobody has reasoned about.
pub fn install_entry(esp: &Path, source: &Path, version: &str) -> io::Result<PathBuf> {
    let directory = esp.join(ENTRY_DIR);
    fs::create_dir_all(&directory)?;

    let name = format!("plexos-{version}+{INITIAL_TRIES}.efi");
    let destination = directory.join(&name);
    fs::copy(source, &destination)?;

    // The ESP is FAT on removable-ish media and this file decides what boots next.
    // Returning before it is on the medium would let a power cut between here and the
    // reboot leave a truncated kernel with a counter that says "try me".
    fs::File::open(&destination)?.sync_all()?;

    Ok(destination)
}

/// Removes boot entries the bootloader has given up on, except the one that is running.
///
/// Found by causing a real rollback. A failed update leaves an exhausted entry — `+0-3` —
/// on the ESP, and nothing ever removed it. ADR-0003 sized the ESP for three UKIs and
/// each of these is 18 MB, so a handful of bad updates fills the one partition the
/// machine cannot boot without. An exhausted entry is the safest thing on the ESP to
/// delete, because it is the one entry that is definitely not a rollback target:
/// `systemd-boot` sorts it below every other entry and will not choose it while anything
/// else exists.
///
/// `running` guards the case where that reasoning fails. A machine can be *running* an
/// exhausted entry — two bad updates in a row leave the bootloader with nothing else and
/// it boots one anyway, which is ADR-0005's "no known-good slot left". Deleting that
/// would remove the boot entry of the system executing the delete, turning an appliance
/// somebody can still reach into one that needs recovery media. So the entry whose
/// version is the running one survives, however dead its counter looks.
///
/// Returns what it removed, so the update log can say so. Every failure is swallowed:
/// this is housekeeping alongside an update, and refusing an update because a dead file
/// would not delete is a worse bargain than a full ESP.
#[must_use]
pub fn remove_wreckage(esp: &Path, running: &str) -> Vec<String> {
    let running_stem = format!("plexos-{running}");
    let mut removed = Vec::new();

    let Ok(entries) = entries(esp) else {
        return removed;
    };

    for (path, entry) in entries {
        if entry.is_exhausted() && entry.stem != running_stem && fs::remove_file(&path).is_ok() {
            removed.push(entry.to_string());
        }
    }

    removed
}

/// Tries a new entry is given before the bootloader gives up on it (ADR-0005).
pub const INITIAL_TRIES: u32 = 3;

/// Mounts the ESP, runs `action`, and unmounts it again.
///
/// The unmount happens whether or not `action` succeeded: an ESP left mounted
/// read-write is the failure mode this whole module is arranged to avoid.
///
/// # Errors
///
/// If the ESP cannot be found or mounted, or if `action` fails. A mount failure is
/// reported with the partition label, since the realistic cause is a disk that was
/// not written by the installer.
pub fn with_esp_mounted<T>(
    device: &str,
    action: &mut dyn FnMut(&Path) -> io::Result<T>,
) -> io::Result<T> {
    fs::create_dir_all(MOUNT_POINT)?;

    plexos_sys::mount::mount(device, MOUNT_POINT, "vfat", "rw,nosuid,nodev,noexec").map_err(
        |error| {
            io::Error::new(
                error.kind(),
                format!(
                    "mounting the ESP ({LABEL_ESP}) from {device}: {error}; \
                     the boot counter cannot be cleared, so this slot will roll back"
                ),
            )
        },
    )?;

    let result = action(Path::new(MOUNT_POINT));
    let unmounted = plexos_sys::mount::unmount(MOUNT_POINT);

    match (result, unmounted) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(io::Error::new(
            error.kind(),
            format!("the counter was cleared but the ESP could not be unmounted: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_entry_is_installed_on_trial_and_the_old_one_is_left_alone() {
        // The old entry is the rollback target, not litter. Removing it would turn a
        // recoverable bad update into an unbootable machine, which is the single failure
        // this whole mechanism exists to prevent.
        let esp = std::env::temp_dir().join("plexos-esp-install");
        let _ = std::fs::remove_dir_all(&esp);
        std::fs::create_dir_all(esp.join(ENTRY_DIR)).unwrap();
        let good = esp.join(ENTRY_DIR).join("plexos-0.1.0.efi");
        std::fs::write(&good, b"the entry that works").unwrap();

        let source = esp.join("new.efi");
        std::fs::write(&source, b"the new kernel").unwrap();

        let installed = install_entry(&esp, &source, "0.1.0.2").unwrap();
        assert_eq!(
            installed.file_name().unwrap(),
            std::ffi::OsStr::new("plexos-0.1.0.2+3.efi"),
            "the counter is the whole of ADR-0005"
        );
        assert!(good.exists(), "the rollback target must survive");
        assert_eq!(std::fs::read(&installed).unwrap(), b"the new kernel");

        let _ = std::fs::remove_dir_all(&esp);
    }

    #[test]
    fn the_installed_entry_parses_as_a_boot_entry_with_its_tries_intact() {
        // install_entry writes the name and bootcounter reads it. If they disagreed, the
        // gate would never find the entry to mark good, and a perfectly healthy slot
        // would roll back.
        let name = format!("plexos-0.1.0.2+{INITIAL_TRIES}.efi");
        let entry = BootEntry::parse(&name).expect("the name this module writes must parse");
        assert_eq!(entry.tries_left, Some(INITIAL_TRIES));
        assert!(entry.is_on_trial());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plexosd-esp-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(ENTRY_DIR)).unwrap();
        dir
    }

    #[test]
    fn an_update_clears_away_the_wreckage_of_a_failed_one() {
        // Found by causing a real rollback. The exhausted entry was left on the ESP with
        // nothing to remove it, on a partition ADR-0003 sized for three UKIs of 18 MB
        // each -- so failed updates accumulate until the thing that decides whether the
        // machine boots runs out of room. The pairing with install_entry mirrors
        // `update::run`, which does both inside one mount of the ESP.
        let esp = scratch("wreckage");
        let dead = esp.join(ENTRY_DIR).join("plexos-0.1.0.1+0-3.efi");
        let good = esp.join(ENTRY_DIR).join("plexos-0.1.0.efi");
        fs::write(&dead, b"three failed boots").unwrap();
        fs::write(&good, b"the entry that works").unwrap();

        let source = esp.join("new.efi");
        fs::write(&source, b"the new kernel").unwrap();

        let cleared = remove_wreckage(&esp, "0.1.0");
        install_entry(&esp, &source, "0.1.0.2").unwrap();

        assert_eq!(cleared, ["plexos-0.1.0.1+0-3.efi"]);
        assert!(!dead.exists(), "the bootloader has given up on this one");
        assert!(good.exists(), "the rollback target must survive");

        let _ = fs::remove_dir_all(&esp);
    }

    #[test]
    fn the_entry_that_is_running_survives_even_when_it_is_exhausted() {
        // The case that makes the rule non-obvious. Two bad updates in a row leave the
        // bootloader with nothing but an entry it has already given up on, and it boots
        // that anyway -- ADR-0005's "no known-good slot left". Removing it here would
        // delete the boot entry of the system doing the deleting, which turns an
        // appliance somebody can still reach into one that needs recovery media.
        let esp = scratch("running-exhausted");
        let running = esp.join(ENTRY_DIR).join("plexos-0.1.0.9+0-3.efi");
        fs::write(&running, b"exhausted, and the only thing that booted").unwrap();

        let cleared = remove_wreckage(&esp, "0.1.0.9");

        assert!(cleared.is_empty(), "nothing was safe to remove");
        assert!(
            running.exists(),
            "never delete the entry this system is running from"
        );

        let _ = fs::remove_dir_all(&esp);
    }

    #[test]
    fn wreckage_removal_reports_what_it_removed_and_leaves_healthy_entries() {
        let esp = scratch("wreckage-report");
        for name in [
            "plexos-0.1.0.efi",     // permanent
            "plexos-0.1.0.5+2.efi", // on trial, still counting
            "plexos-0.1.0.1+0-3.efi",
            "plexos-0.1.0.2+0-3.efi",
        ] {
            fs::write(esp.join(ENTRY_DIR).join(name), b"x").unwrap();
        }

        let mut removed = remove_wreckage(&esp, "0.1.0");
        removed.sort();
        assert_eq!(
            removed,
            ["plexos-0.1.0.1+0-3.efi", "plexos-0.1.0.2+0-3.efi"]
        );

        assert!(esp.join(ENTRY_DIR).join("plexos-0.1.0.efi").exists());
        assert!(
            esp.join(ENTRY_DIR).join("plexos-0.1.0.5+2.efi").exists(),
            "an entry with tries left is a live candidate, not wreckage"
        );

        let _ = fs::remove_dir_all(&esp);
    }

    #[test]
    fn entries_are_found_and_parsed() {
        let esp = scratch("entries");
        for name in ["plexos-0.1.0+3.efi", "plexos-0.0.9.efi", "loader.conf"] {
            fs::write(esp.join(ENTRY_DIR).join(name), b"x").unwrap();
        }
        let found = entries(&esp).unwrap();
        assert_eq!(found.len(), 2, "loader.conf is not a boot entry");
        let _ = fs::remove_dir_all(&esp);
    }

    #[test]
    fn marking_good_renames_the_file_on_disk() {
        let esp = scratch("mark");
        let path = esp.join(ENTRY_DIR).join("plexos-0.1.0+3.efi");
        fs::write(&path, b"uki").unwrap();

        let entry = BootEntry::parse("plexos-0.1.0+3.efi").unwrap();
        let renamed = mark_good(&path, &entry).unwrap().unwrap();

        assert!(!path.exists(), "the counted name must be gone");
        assert!(renamed.exists());
        assert_eq!(renamed.file_name().unwrap(), "plexos-0.1.0.efi");
        assert_eq!(fs::read(&renamed).unwrap(), b"uki", "contents preserved");
        let _ = fs::remove_dir_all(&esp);
    }

    #[test]
    fn marking_an_already_good_entry_does_nothing() {
        let esp = scratch("idempotent");
        let path = esp.join(ENTRY_DIR).join("plexos-0.1.0.efi");
        fs::write(&path, b"uki").unwrap();

        let entry = BootEntry::parse("plexos-0.1.0.efi").unwrap();
        assert_eq!(mark_good(&path, &entry).unwrap(), None);
        assert!(path.exists(), "the file must be left alone");
        let _ = fs::remove_dir_all(&esp);
    }

    #[test]
    fn an_exhausted_entry_is_refused() {
        // The bootloader has already skipped this one. Renaming it would make a
        // three-times-failed image permanent.
        let esp = scratch("exhausted");
        let path = esp.join(ENTRY_DIR).join("plexos-0.1.0+0-3.efi");
        fs::write(&path, b"uki").unwrap();

        let entry = BootEntry::parse("plexos-0.1.0+0-3.efi").unwrap();
        let error = mark_good(&path, &entry).unwrap_err();
        assert!(error.to_string().contains("resurrect"), "{error}");
        assert!(path.exists(), "the file must not be renamed");
        let _ = fs::remove_dir_all(&esp);
    }
}
