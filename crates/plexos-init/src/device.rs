//! Finding a partition by its GPT label, without `udev`.
//!
//! ADR-0003 puts slot identity in the partition *label*, and the boot plan therefore
//! names devices as `/dev/disk/by-partlabel/usr_a`. That path is a symlink created by
//! `udev`, and the initrd has no `udev` — that is the point of it (ARCHITECTURE.md
//! §3). `devtmpfs` gives us `/dev/vda1` and nothing that says which slot it holds.
//!
//! The first image built failed here, several steps into a boot that was otherwise
//! working: "opening verity hash device /dev/disk/by-partlabel/usr_a_hash: No such
//! file or directory". The labels were correct; the symlink farm simply did not
//! exist. It is the same absence that made [`plexos_sys::dm`] create
//! `/dev/mapper/<name>` itself.
//!
//! The kernel already knows the answer. Its GPT parser copies each partition name
//! into `volname` (`block/partitions/efi.c`) and exposes it as `PARTNAME` in the
//! partition's uevent (`block/partitions/core.c`), which sysfs regenerates whenever
//! `/sys/class/block/<dev>/uevent` is read. So the mapping from label to device is
//! there for the asking, with no daemon and no waiting.
//!
//! Nothing here is unsafe: it is reading text files out of sysfs.

use std::fmt;
use std::fs;
use std::io;

/// Where the kernel publishes one directory per block device.
const SYS_BLOCK: &str = "/sys/class/block";

/// A partition, as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// Kernel device name, e.g. `vda2`.
    pub devname: String,
    /// GPT partition label, e.g. `usr_a`.
    pub partname: String,
}

impl fmt::Display for Partition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (/dev/{})", self.partname, self.devname)
    }
}

/// Extracts `DEVNAME` and `PARTNAME` from the contents of a sysfs `uevent` file.
///
/// Returns `None` for anything that is not a named partition: whole disks have no
/// `PARTNAME`, and neither do partitions on a disk without GPT labels.
#[must_use]
pub fn parse_uevent(contents: &str) -> Option<Partition> {
    let mut devname = None;
    let mut partname = None;

    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("DEVNAME=") {
            devname = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("PARTNAME=") {
            partname = Some(value.trim().to_owned());
        }
    }

    match (devname, partname) {
        (Some(devname), Some(partname)) if !partname.is_empty() => {
            Some(Partition { devname, partname })
        }
        _ => None,
    }
}

/// Every labelled partition the kernel currently knows about.
///
/// # Errors
///
/// If `/sys/class/block` cannot be read, which means `sysfs` is not mounted — a
/// programming error in the boot plan rather than a condition to recover from.
pub fn labelled_partitions() -> io::Result<Vec<Partition>> {
    let mut found = Vec::new();

    for entry in fs::read_dir(SYS_BLOCK)? {
        let path = entry?.path().join("uevent");
        // A device can disappear between listing and reading; that is not an error,
        // it simply is not the partition we are looking for.
        if let Ok(contents) = fs::read_to_string(&path)
            && let Some(partition) = parse_uevent(&contents)
        {
            found.push(partition);
        }
    }

    found.sort_by(|a, b| a.devname.cmp(&b.devname));
    Ok(found)
}

/// Resolves a GPT partition label to the device node `devtmpfs` created for it.
///
/// # Errors
///
/// If no partition carries the label. The message lists what *was* found, because the
/// realistic causes — an image written by a tool that dropped GPT labels, or a disk
/// that is not a PlexOS disk at all — are indistinguishable without that list.
pub fn by_partlabel(label: &str) -> io::Result<String> {
    let partitions = labelled_partitions()?;

    if let Some(found) = partitions.iter().find(|p| p.partname == label) {
        return Ok(format!("/dev/{}", found.devname));
    }

    let seen = if partitions.is_empty() {
        "no labelled partitions at all".to_owned()
    } else {
        partitions
            .iter()
            .map(|p| p.partname.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no partition labelled {label:?}; the kernel reports: {seen}. \
             Slot identity lives in the GPT label (ADR-0003), so a disk written by a \
             tool that drops labels is unbootable and must be rewritten with the \
             installer."
        ),
    ))
}

/// Rewrites a `by-partlabel` path into a real device node, leaving others alone.
///
/// The boot plan names devices the way a person would write them, and that form is
/// what `--dry-run` prints. Resolving happens here, at the moment of use, so the plan
/// stays a pure function of the command line.
///
/// # Errors
///
/// If the path names a label that no partition carries.
pub fn resolve(path: &str) -> io::Result<String> {
    match path.strip_prefix("/dev/disk/by-partlabel/") {
        Some(label) => by_partlabel(label),
        None => Ok(path.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a real partition uevent, as the kernel emits it.
    const PARTITION: &str = "MAJOR=254\nMINOR=2\nDEVNAME=vda2\nDEVTYPE=partition\n\
                             DISKSEQ=1\nPARTN=2\nPARTNAME=usr_a\n\
                             PARTUUID=8484680c-9521-48c6-9c11-b0720656f69e\n";

    /// A whole disk. No PARTNAME, and it must not be mistaken for a partition.
    const WHOLE_DISK: &str = "MAJOR=254\nMINOR=0\nDEVNAME=vda\nDEVTYPE=disk\nDISKSEQ=1\n";

    /// A partition on a disk whose GPT carries no names.
    const UNNAMED: &str = "MAJOR=8\nMINOR=1\nDEVNAME=sda1\nDEVTYPE=partition\nPARTN=1\n";

    #[test]
    fn a_labelled_partition_is_recognised() {
        assert_eq!(
            parse_uevent(PARTITION),
            Some(Partition {
                devname: "vda2".to_owned(),
                partname: "usr_a".to_owned(),
            })
        );
    }

    #[test]
    fn a_whole_disk_is_not_a_partition() {
        assert_eq!(parse_uevent(WHOLE_DISK), None);
    }

    #[test]
    fn an_unlabelled_partition_is_ignored() {
        // Matching these would resolve a label to whichever disk happened to be
        // enumerated first, which is a far worse failure than not finding it.
        assert_eq!(parse_uevent(UNNAMED), None);
    }

    #[test]
    fn an_empty_partname_does_not_count_as_a_label() {
        assert_eq!(parse_uevent("DEVNAME=vda1\nPARTNAME=\n"), None);
    }

    #[test]
    fn a_prefix_of_a_key_is_not_the_key() {
        // "PARTN=2" starts the same way as "PARTNAME="; a `contains` or a sloppy
        // prefix match would read the partition number as the label.
        let parsed = parse_uevent(PARTITION).unwrap();
        assert_eq!(
            parsed.partname, "usr_a",
            "PARTN must not be read as PARTNAME"
        );
    }

    #[test]
    fn resolve_passes_through_paths_that_are_not_labels() {
        assert_eq!(
            resolve("/dev/mapper/plexos-usr").unwrap(),
            "/dev/mapper/plexos-usr"
        );
        assert_eq!(resolve("tmpfs").unwrap(), "tmpfs");
    }

    #[test]
    fn a_missing_label_reports_what_was_found_instead() {
        // On a developer machine there are no PlexOS labels, so this exercises the
        // real failure text. "No such file or directory" alone gives nothing to act
        // on; the list distinguishes "wrong disk" from "labels were dropped".
        let error = by_partlabel("usr_a").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let text = error.to_string();
        assert!(text.contains("usr_a"), "{text}");
        assert!(text.contains("kernel reports"), "{text}");
        assert!(text.contains("installer"), "no remedy given: {text}");
    }

    #[test]
    fn scanning_sysfs_works_on_the_machine_running_the_tests() {
        // Not asserting what is found -- that depends on the machine -- only that
        // the scan itself succeeds against a real /sys/class/block.
        assert!(labelled_partitions().is_ok());
    }
}
