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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plexosd-esp-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(ENTRY_DIR)).unwrap();
        dir
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
