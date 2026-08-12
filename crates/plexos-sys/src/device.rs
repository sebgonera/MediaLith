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
//! exist. It is the same absence that made [`crate::dm`] create
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
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Where the kernel publishes one directory per block device.
const SYS_BLOCK: &str = "/sys/class/block";

/// A partition, as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// Kernel device name, e.g. `vda2`.
    pub devname: String,
    /// GPT partition label, e.g. `usr_a`.
    pub partname: String,
    /// GPT unique partition GUID, lower case.
    ///
    /// The only identifier here that is unique across disks. Labels are not — an installed
    /// MediaLith with its installer stick attached has two partitions called `esp` — and this
    /// is what `systemd-boot` reports in `LoaderDevicePartUUID` to say which one the
    /// firmware actually booted.
    pub partuuid: String,
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
    let mut partuuid = String::new();

    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("DEVNAME=") {
            devname = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("PARTNAME=") {
            partname = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("PARTUUID=") {
            partuuid = value.trim().to_ascii_lowercase();
        }
    }

    match (devname, partname) {
        (Some(devname), Some(partname)) if !partname.is_empty() => Some(Partition {
            devname,
            partname,
            partuuid,
        }),
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

/// The whole disk a partition device name belongs to: `sda4` is `sda`, `nvme0n1p2` is
/// `nvme0n1`.
///
/// **Takes a partition name.** A disk name cannot be told from a partition name by looking
/// at it — `nvme0n1` is a disk, `sda1` is a partition, and both end in a digit.
#[must_use]
pub fn disk_of(partition: &str) -> String {
    let stem = partition.trim_end_matches(|c: char| c.is_ascii_digit());
    if stem.ends_with('p') && stem[..stem.len() - 1].ends_with(|c: char| c.is_ascii_digit()) {
        return stem[..stem.len() - 1].to_owned();
    }
    stem.to_owned()
}

/// Where the firmware records what it booted, once `efivarfs` is mounted.
///
/// The name is fixed by the Boot Loader Interface: the vendor GUID is systemd's and the
/// variable is set by `systemd-boot` itself.
pub const LOADER_DEVICE_PART_UUID: &str =
    "LoaderDevicePartUUID-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// Reads the partition GUID of the ESP the firmware booted from.
///
/// # The only authoritative answer at boot
///
/// Everything else MediaLith could ask is ambiguous once a machine has two MediaLith disks: the
/// labels are duplicated, and the running system has not been assembled yet so there is no
/// device-mapper device to work back from. `systemd-boot` knows, because it is the thing
/// the firmware loaded, and it writes the answer here.
///
/// An EFI variable begins with four bytes of attributes; the value is UTF-16. Both are
/// handled by taking every ASCII character after the first four bytes, which is enough for
/// a GUID and refuses to be clever about anything else.
///
/// `None` when the variable is absent — booted by something other than `systemd-boot`, or
/// `efivarfs` not mounted. Callers must fall back rather than fail: a machine with one disk
/// has always booted correctly without this.
#[must_use]
pub fn booted_partuuid(efivars: &str) -> Option<String> {
    let raw = fs::read(format!("{efivars}/{LOADER_DEVICE_PART_UUID}")).ok()?;
    let text: String = raw
        .get(4..)?
        .iter()
        .filter(|b| b.is_ascii_graphic())
        .map(|b| char::from(*b))
        .collect();

    (text.len() == 36).then(|| text.to_ascii_lowercase())
}

/// The disk carrying the partition with this GUID.
///
/// # Errors
/// If no partition carries it, which means the firmware booted something this kernel
/// cannot see — a disk unplugged between the bootloader and now, most plausibly.
pub fn disk_with_partuuid(partuuid: &str) -> io::Result<String> {
    let wanted = partuuid.to_ascii_lowercase();
    labelled_partitions()?
        .iter()
        .find(|p| p.partuuid == wanted)
        .map(|p| disk_of(&p.devname))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no partition has the GUID {wanted}, which is the one the boot loader \
                     says it started from. Remedy: the disk it booted is not visible to \
                     this kernel -- unplugged between the bootloader and now, or a \
                     controller this kernel has no driver for."
                ),
            )
        })
}

/// Resolves a label to a device, considering only partitions on `disk`.
///
/// # Why a label alone is not a question with one answer
///
/// It was, for as long as a MediaLith machine had one MediaLith disk. The installer ended that:
/// a machine with the system on its internal drive and the USB stick still plugged in has
/// **two** partitions labelled `esp`, two labelled `usr_a`, and two labelled `var`.
/// [`by_partlabel`] returns whichever the kernel enumerated first, and that call chooses
/// the partition an update is *written to* and the ESP a boot entry is installed on.
///
/// Observed, not theorised: an update installed in that state landed on a disk nothing in
/// the code had chosen. It was the right one, and nothing made it so.
///
/// # Errors
/// If `disk` has no partition with that label. The message names the disk, because "no
/// partition labelled `usr_a`" is confusing on a machine that visibly has one.
pub fn by_partlabel_on(disk: &str, label: &str) -> io::Result<String> {
    let partitions = labelled_partitions()?;

    if let Some(found) = partitions
        .iter()
        .find(|p| p.partname == label && disk_of(&p.devname) == disk)
    {
        return Ok(format!("/dev/{}", found.devname));
    }

    let elsewhere: Vec<&Partition> = partitions.iter().filter(|p| p.partname == label).collect();
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{disk} has no partition labelled {label:?}{}. Remedy: this is the disk the \
             running system is on, and it is the only one that may be written -- a label \
             on another disk belongs to another installation.",
            if elsewhere.is_empty() {
                String::new()
            } else {
                format!(
                    ", though {} does",
                    elsewhere
                        .iter()
                        .map(|p| p.devname.clone())
                        .collect::<Vec<_>>()
                        .join(" and ")
                )
            }
        ),
    ))
}

/// Resolves a GPT partition label to the device node `devtmpfs` created for it.
///
/// **Ambiguous once more than one MediaLith disk is attached**, which an installed machine
/// with its installer stick still in it has. Prefer [`by_partlabel_on`] wherever the disk
/// is known, and it is known wherever something is about to be written.
///
/// # Errors
///
/// If no partition carries the label. The message lists what *was* found, because the
/// realistic causes — an image written by a tool that dropped GPT labels, or a disk
/// that is not a MediaLith disk at all — are indistinguishable without that list.
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

/// How long to wait for a labelled partition to appear before giving up.
///
/// Not a guess at how slow a disk is, but at how slow *enumeration* is. PCI devices
/// are there before `plexos-init` runs; USB ones are not, and can take several
/// seconds — the same lateness that ARCHITECTURE.md §2 says must never be allowed to
/// influence the boot health gate. Booting an appliance from a USB stick is a
/// first-class case here, so the boot has to be willing to wait for one.
///
/// Long enough for a slow hub and a spinning USB disk; short enough that a genuinely
/// absent disk fails the slot rather than hanging forever, which would defeat the
/// rollback in ADR-0005 by never failing at all.
pub const DEVICE_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between rescans while waiting.
const POLL: Duration = Duration::from_millis(100);

/// Waits for a labelled partition to appear, then resolves it.
///
/// Returns immediately when the partition is already present, which is the case on
/// any internal disk.
///
/// # Errors
///
/// If the label has not appeared within `timeout`. The message distinguishes "never
/// appeared" from "appeared but wrong", because on removable media the first is
/// usually a device that was not plugged in and the second is the wrong stick.
pub fn wait_for_partlabel(
    label: &str,
    timeout: Duration,
    log: &mut dyn FnMut(&str),
) -> io::Result<String> {
    let deadline = Instant::now() + timeout;
    let mut announced = false;

    loop {
        match by_partlabel(label) {
            Ok(device) => return Ok(device),
            Err(error) if Instant::now() >= deadline => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("waited {}s: {error}", timeout.as_secs()),
                ));
            }
            Err(_) => {}
        }

        // Said once, not every 100ms, and only when there is actually a wait --
        // otherwise every boot from an internal disk would print it.
        if !announced {
            announced = true;
            log(&format!(
                "waiting for partition {label} to appear (USB storage enumerates \
                 seconds after PCI)"
            ));
        }
        sleep(POLL);
    }
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
pub fn resolve(path: &str, log: &mut dyn FnMut(&str)) -> io::Result<String> {
    resolve_on(None, path, log)
}

/// [`resolve`], preferring partitions on `disk`.
///
/// `None` means "any disk", which is what a machine with one MediaLith disk has always meant
/// and what this did before an installer existed.
///
/// # Errors
/// As [`resolve`], and additionally if `disk` is given and carries no such label — which is
/// a machine whose boot disk is missing a partition its own boot loader expected.
pub fn resolve_on(disk: Option<&str>, path: &str, log: &mut dyn FnMut(&str)) -> io::Result<String> {
    let Some(label) = path.strip_prefix("/dev/disk/by-partlabel/") else {
        return Ok(path.to_owned());
    };

    let Some(disk) = disk else {
        // Nothing to scope to: one-disk machines, and firmware that does not publish which
        // device it booted. Waiting for the label on any disk is all there is, and is what
        // this has always done.
        return wait_for_partlabel(label, DEVICE_TIMEOUT, log);
    };

    // Waited for **on the disk the firmware booted**, and this ordering is the whole fix.
    //
    // It used to wait for the label on any disk and only then prefer the booted one, which
    // reads as harmless and is a race. A machine booting from a USB stick with a second
    // MediaLith disk attached enumerates the internal disk first: at 8.6 s the leftover
    // 500 GB disk had `usr_a`, at 8.7 s this had already resolved to it and started
    // dm-verity against it, and at 9.7 s -- a second later -- the stick's own partitions
    // appeared. The boot failed with "metadata block 1 is corrupted", which is what
    // verifying one installation's `/usr` against another's hash tree looks like.
    //
    // The fallback made it worse rather than saving it: `by_partlabel_on` could not find the
    // partition on a stick that did not exist yet, so the code took the other disk's
    // deliberately. For `/usr` that is a failed boot; for `/var` it is silent, and the
    // machine comes up on another installation's Plex database, device token and
    // certificate with nothing reporting anything wrong.
    wait_for_partlabel_on(disk, label, DEVICE_TIMEOUT, log)
}

/// Waits for a labelled partition **on one disk**, and refuses every other disk's.
///
/// # Errors
/// If the disk has no partition with that label before the timeout. It does not fall back
/// to another disk: a `usr_a` belonging to a different installation is not a worse answer
/// than none, it is a wrong one, and the `/var` case is wrong without saying so.
pub fn wait_for_partlabel_on(
    disk: &str,
    label: &str,
    timeout: Duration,
    log: &mut dyn FnMut(&str),
) -> io::Result<String> {
    let deadline = Instant::now() + timeout;
    let mut announced = false;

    loop {
        if let Ok(device) = by_partlabel_on(disk, label) {
            return Ok(device);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "waited {}s for partition {label} on {disk}, which is the disk the \
                     firmware booted. It never appeared. Another disk may carry that label \
                     and this deliberately did not use it: mounting another installation's \
                     partition is a wrong answer rather than a slow one. Remedy: if this \
                     machine has a second MediaLith disk attached, unplug it and restart; \
                     if not, the boot disk's partition table is not what this release \
                     expects.",
                    timeout.as_secs()
                ),
            ));
        }

        if !announced {
            announced = true;
            log(&format!(
                "waiting for partition {label} on {disk} (USB storage enumerates seconds \
                 after PCI, so the disk this booted from may not be here yet)"
            ));
        }
        sleep(POLL);
    }
}

/// Where `efivarfs` is mounted while the boot disk is being identified.
const EFIVARS_MOUNT: &str = "/run/plexos-efivars";

/// The disk the firmware booted from, if it can be established.
///
/// Mounts `efivarfs`, reads what `systemd-boot` left there, and unmounts it again. Done
/// here rather than as a boot-plan step because it is a question, not a piece of the
/// assembled system: nothing after this needs `efivarfs` and leaving it mounted would put
/// the firmware's variable store inside a running appliance for no reason.
///
/// `None` for every failure, each logged. A machine with one MediaLith disk has always booted
/// without this, and turning "I could not ask" into a failed boot would be a worse trade
/// than the ambiguity it removes.
#[must_use]
pub fn booted_disk(log: &mut dyn FnMut(&str)) -> Option<String> {
    if let Err(error) = fs::create_dir_all(EFIVARS_MOUNT) {
        log(&format!("could not make {EFIVARS_MOUNT}: {error}"));
        return None;
    }

    if let Err(error) = crate::mount::mount("efivarfs", EFIVARS_MOUNT, "efivarfs", "nosuid,nodev") {
        log(&format!(
            "efivarfs would not mount ({error}), so which disk the firmware booted is \
             unknown. Partitions are resolved by label alone, which is what this did \
             before."
        ));
        return None;
    }

    let found = booted_partuuid(EFIVARS_MOUNT);
    let _ = crate::mount::unmount(EFIVARS_MOUNT);

    let partuuid = found?;
    match disk_with_partuuid(&partuuid) {
        Ok(disk) => {
            log(&format!(
                "the firmware booted {disk} (partition {partuuid})"
            ));
            Some(disk)
        }
        Err(error) => {
            log(&format!("{error}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_on_the_wrong_disk_is_refused_rather_than_used() {
        // The boot this exists to stop, from a real console log. A USB stick with a second
        // MediaLith disk attached: at 8.6 s the internal disk's `usr_a` existed, at 8.7 s
        // PID 1 had resolved to it and started dm-verity, and at 9.7 s the stick's own
        // partitions appeared. "metadata block 1 is corrupted" is what verifying one
        // installation's /usr against another's hash tree looks like.
        //
        // The old code waited for the label on *any* disk and only then preferred the booted
        // one, so the race was already lost by the time the preference was applied -- and
        // when the preference could not be satisfied it took the other disk deliberately.
        //
        // A disk that does not exist stands in for one that has not enumerated yet: both are
        // "no such partition on this disk", which is the state that used to fall through.
        let mut said = Vec::new();
        let error = wait_for_partlabel_on(
            "nonexistent-disk",
            "usr_a",
            Duration::from_millis(20),
            &mut |m| said.push(m.to_owned()),
        )
        .expect_err("a partition that is not on this disk must not resolve to another's");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let message = error.to_string();
        assert!(
            message.contains("firmware booted"),
            "the refusal has to say which disk it was looking at: {message}"
        );
        assert!(
            message.contains("Remedy:"),
            "and name a remedy, like every other diagnostic here: {message}"
        );
        assert!(
            message.contains("unplug"),
            "the remedy for the boot this came from is to remove the other disk: {message}"
        );
    }

    #[test]
    fn without_a_booted_disk_the_old_behaviour_is_what_is_left() {
        // `None` is not a failure: it is every one-disk machine, and every firmware that
        // does not publish which device it booted. There is nothing to scope to, so the
        // label alone is all there is -- and that has to keep working, or the fix for a
        // two-disk machine would stop a one-disk machine booting.
        //
        // A path that is not a by-partlabel path is returned untouched, which is the branch
        // every non-label device takes.
        let mut said = Vec::new();
        assert_eq!(
            resolve_on(None, "/dev/vda2", &mut |m| said.push(m.to_owned())).unwrap(),
            "/dev/vda2"
        );
        assert_eq!(
            resolve_on(Some("vda"), "/dev/mapper/plexos-usr", &mut |m| said
                .push(m.to_owned()))
            .unwrap(),
            "/dev/mapper/plexos-usr"
        );
    }

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
                partuuid: "8484680c-9521-48c6-9c11-b0720656f69e".to_owned(),
            })
        );
    }

    #[test]
    fn a_partition_guid_is_read_from_the_same_uevent_as_the_label() {
        // The fixture is a real one, and this is the field that makes a partition
        // identifiable across disks when its label no longer is. No extra tool is needed
        // for it -- which is the whole reason PID 1 can use it before anything is mounted.
        let partition = parse_uevent(PARTITION).expect("a partition");
        assert_eq!(partition.partuuid, "8484680c-9521-48c6-9c11-b0720656f69e");
        assert_eq!(
            partition.partuuid,
            partition.partuuid.to_ascii_lowercase(),
            "compared against an EFI variable that is upper case, so one side has to be \
             normalised and it is this one"
        );

        // A disk whose GPT carries no names carries no GUID here either, and must not be
        // mistaken for one with an empty GUID that could match an empty search.
        assert_eq!(parse_uevent(UNNAMED), None);
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
        let mut log = |_: &str| {};
        assert_eq!(
            resolve("/dev/mapper/plexos-usr", &mut log).unwrap(),
            "/dev/mapper/plexos-usr"
        );
        assert_eq!(resolve("tmpfs", &mut log).unwrap(), "tmpfs");
    }

    #[test]
    fn a_pass_through_path_does_not_wait() {
        // Only by-partlabel paths may block. If this ever started waiting, every
        // tmpfs mount in the plan would add the full timeout to the boot.
        let start = std::time::Instant::now();
        let mut log = |_: &str| {};
        let _ = resolve("tmpfs", &mut log);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn waiting_for_a_label_that_never_appears_times_out_rather_than_hanging() {
        // A hang here would be worse than a failure: the slot would never be marked
        // bad, and the rollback in ADR-0005 depends on failing boots actually failing.
        let mut messages = Vec::new();
        let start = std::time::Instant::now();
        let error = wait_for_partlabel("no-such-label", Duration::from_millis(300), &mut |m| {
            messages.push(m.to_owned());
        })
        .unwrap_err();

        assert!(start.elapsed() < Duration::from_secs(5), "took too long");
        assert!(error.to_string().contains("waited"), "{error}");
        assert_eq!(
            messages.len(),
            1,
            "the wait should be announced exactly once"
        );
        assert!(messages[0].contains("USB"), "{:?}", messages[0]);
    }

    #[test]
    fn a_missing_label_reports_what_was_found_instead() {
        // A label no disk can carry, rather than a real one that happens to be absent.
        //
        // This asked about `usr_a` and said "on a developer machine there are no MediaLith
        // labels", which was true of the machine it was written on and stopped being true
        // the moment somebody wrote an image to a USB stick and left it plugged in: the
        // build host then has `sda1..sda6` labelled `esp`, `usr_a`, `usr_a_hash`, and the
        // test fails on a machine where nothing is wrong. Third time this repository has
        // produced a test that describes the machine it was written on.
        //
        // What is being tested is the failure *text*, and an impossible label exercises it
        // on every host and on any day.
        let error = by_partlabel("medialith-no-such-label").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let text = error.to_string();
        assert!(text.contains("medialith-no-such-label"), "{text}");
        assert!(text.contains("kernel reports"), "{text}");
        assert!(text.contains("installer"), "no remedy given: {text}");
    }

    #[test]
    fn scanning_sysfs_works_on_the_machine_running_the_tests() {
        // Not asserting what is found -- that depends on the machine -- only that
        // the scan itself succeeds against a real /sys/class/block.
        assert!(labelled_partitions().is_ok());
    }

    #[test]
    fn a_partition_resolves_to_the_disk_it_is_on() {
        assert_eq!(disk_of("sda4"), "sda");
        assert_eq!(disk_of("nvme0n1p2"), "nvme0n1");
        assert_eq!(disk_of("mmcblk0p1"), "mmcblk0");
        assert_eq!(disk_of("vda1"), "vda");
    }

    #[test]
    fn a_label_alone_stopped_being_a_question_with_one_answer() {
        // The installer is what ended it. A machine with MediaLith on its internal disk and
        // the stick it was installed from still plugged in carries two of every label, and
        // the label is what chooses the partition an update is written to.
        //
        // This pins the *reason* rather than the mechanism: `by_partlabel_on` exists so
        // that the question carries a disk, and deleting it would put the ambiguity back.
        let both = [
            Partition {
                devname: "sda2".to_owned(),
                partname: "usr_a".to_owned(),
                partuuid: "07986889-de27-4a75-841a-274080495d3b".to_owned(),
            },
            Partition {
                devname: "nvme0n1p2".to_owned(),
                partname: "usr_a".to_owned(),
                partuuid: "1ded502c-1540-504e-8a48-4974bd6de884".to_owned(),
            },
        ];

        let on = |disk: &str| {
            both.iter()
                .find(|p| p.partname == "usr_a" && disk_of(&p.devname) == disk)
                .map(|p| p.devname.clone())
        };
        assert_eq!(on("sda").as_deref(), Some("sda2"));
        assert_eq!(on("nvme0n1").as_deref(), Some("nvme0n1p2"));
        assert_eq!(on("vdb"), None, "and a disk that has none says so");
    }
}
