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
/// Nor does it remove anything else. [`remove_superseded`] does that, and the update path
/// calls it first — first, because a partition with no room left is how this was found:
/// the copy below failed halfway and left a truncated kernel carrying a try counter.
///
/// # Errors
/// If the entry directory cannot be created or the file cannot be written. A partial
/// write is left behind rather than cleaned up: it carries a `+3` counter and a version
/// that has never booted, so the bootloader will try it, fail, and fall back — whereas a
/// cleanup path that itself failed halfway is a state nobody has reasoned about.
pub fn install_entry(esp: &Path, source: &Path, version: &str) -> io::Result<PathBuf> {
    let directory = esp.join(ENTRY_DIR);
    fs::create_dir_all(&directory)?;

    // Legacy `plexos-` prefix, retained after the MediaLith product rename, and this one
    // has a trap in it worth stating plainly.
    //
    // **The entry for a release is written by the release installing it, not by the one
    // that will boot from it.** So a MediaLith bundle installed by a machine still running
    // PlexOS gets a `plexos-` name whatever this build calls itself. `gate::decide_trial`
    // then looks for its own entry by this same prefix; if the two ever disagree it finds
    // nothing, declines to clear the try counter -- and the entry decays `+2-1`, `+1-2`,
    // `+0-3` until the bootloader gives up and falls back. A healthy machine rolling itself
    // back three reboots later, looking like a hardware fault.
    //
    // The name is invisible to anybody who is not mounting the ESP. It stays.
    let name = format!("plexos-{version}+{INITIAL_TRIES}.efi");
    let destination = directory.join(&name);
    fs::copy(source, &destination)?;

    // The ESP is FAT on removable-ish media and this file decides what boots next.
    // Returning before it is on the medium would let a power cut between here and the
    // reboot leave a truncated kernel with a counter that says "try me".
    fs::File::open(&destination)?.sync_all()?;

    Ok(destination)
}

/// Removes every boot entry that no slot can boot, keeping only the running one.
///
/// # Why almost everything can go
///
/// There are two slots. Called from the update path this runs *after* both partitions
/// have been written, so at that moment the disk holds exactly two versions of `/usr`: the
/// one running, and the one just written. An entry naming any other version points at a
/// filesystem that has been overwritten — choosing it means a dm-verity failure at boot,
/// three tries burnt, and a fallback. Those entries are not merely wasteful; they are the
/// only entries on the partition that are guaranteed not to work.
///
/// The entry for the version being installed is removed too, and then written fresh by
/// [`install_entry`]. A stale one for the same version would differ only in its try
/// counter, which is the one part of the name that decides whether the bootloader still
/// believes in it.
///
/// # Why the running entry survives, whatever its counter says
///
/// It is the system executing this. A machine can be *running* an exhausted entry — two
/// bad updates in a row leave the bootloader with nothing else and it boots one anyway,
/// which is ADR-0005's "no known-good slot left". Deleting that turns an appliance
/// somebody can still reach into one that needs recovery media.
///
/// # What this cost before it existed
///
/// [`install_entry`] never removes the entry that works, which is right, and nothing
/// removed the ones before it, which was not. **The reference laptop reached 25 entries
/// and a 511 MB ESP that was 100% full**, on a partition ADR-0003 sized for three. The
/// update that found it failed with `ENOSPC` while copying, leaving a truncated 664 KB
/// file called `plexos-0.1.0.202607301319+3.efi` — the highest version on the partition,
/// with a full try counter, so `systemd-boot` would have chosen it first and spent three
/// boots discovering it was not a kernel.
///
/// Returns what it removed, so the update log can say so. Every failure is swallowed:
/// this is housekeeping alongside an update, and refusing an update because a dead file
/// would not delete is a worse bargain than a full ESP.
#[must_use]
pub fn remove_superseded(esp: &Path, running: &str) -> Vec<String> {
    let running_stem = format!("plexos-{running}");
    let mut removed = Vec::new();

    let Ok(entries) = entries(esp) else {
        return removed;
    };

    for (path, entry) in entries {
        if entry.stem != running_stem && fs::remove_file(&path).is_ok() {
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

        let cleared = remove_superseded(&esp, "0.1.0");
        install_entry(&esp, &source, "0.1.0.2").unwrap();

        assert_eq!(cleared, ["plexos-0.1.0.1+0-3.efi"]);
        assert!(!dead.exists(), "the bootloader has given up on this one");
        assert!(good.exists(), "the entry that is running must survive");

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

        let cleared = remove_superseded(&esp, "0.1.0.9");

        assert!(cleared.is_empty(), "nothing was safe to remove");
        assert!(
            running.exists(),
            "never delete the entry this system is running from"
        );

        let _ = fs::remove_dir_all(&esp);
    }

    #[test]
    fn everything_but_the_running_entry_goes_however_healthy_it_looks() {
        // Including the one that looks most alive. `plexos-0.1.0.5+2.efi` is on trial with
        // two tries left, so under the old rule it survived as "a live candidate" -- but by
        // the time this runs, the update has already written both partitions, and the slot
        // that held 0.1.0.5 is the slot it just overwrote. The entry is a candidate for a
        // filesystem that no longer exists: choosing it fails dm-verity and burns two
        // boots.
        //
        // Counting is what made this matter. 25 entries filled a 511 MB ESP on the
        // reference laptop, and the update that found it ran out of room mid-copy.
        let esp = scratch("superseded");
        for name in [
            "plexos-0.1.0.efi",     // running
            "plexos-0.1.0.5+2.efi", // staged, and its slot has just been overwritten
            "plexos-0.1.0.1+0-3.efi",
            "plexos-0.1.0.2+0-3.efi",
        ] {
            fs::write(esp.join(ENTRY_DIR).join(name), b"x").unwrap();
        }

        let mut removed = remove_superseded(&esp, "0.1.0");
        removed.sort();
        assert_eq!(
            removed,
            [
                "plexos-0.1.0.1+0-3.efi",
                "plexos-0.1.0.2+0-3.efi",
                "plexos-0.1.0.5+2.efi",
            ]
        );

        assert_eq!(
            fs::read_dir(esp.join(ENTRY_DIR)).unwrap().count(),
            1,
            "one slot is running and the other is about to be written; nothing else can \
             boot, so nothing else may stay"
        );
        assert!(esp.join(ENTRY_DIR).join("plexos-0.1.0.efi").exists());

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
