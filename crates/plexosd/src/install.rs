//! Putting MediaLith on a disk (ADR-0016).
//!
//! The appliance boots from a USB stick, prints a URL, and from a browser somebody chooses
//! a disk. What gets written is **the system that is running** — its `/usr`, its verity
//! tree, and the ESP it booted from — so there is no second artefact to build or keep in
//! step, and what lands on the disk is what dm-verity has already verified and this
//! hardware has already booted.
//!
//! # This is the most destructive thing in the repository
//!
//! Everything else here writes to a partition MediaLith owns. This erases a whole disk that
//! may belong to somebody's computer, and the machine it was written for has Windows on its
//! internal drive. So the shape of this module is refusals: [`candidates`] reads, [`vet`]
//! decides, and only [`install`] writes.
//!
//! Two of those refusals are structural rather than advisory:
//!
//! - **The disk the installer is running from is never a candidate.** Found by resolving
//!   the partitions this system has mounted, not by trusting the `removable` flag — which
//!   is a property of the enclosure and is wrong for, among other things, an internal SD
//!   reader.
//! - **A disk with anything on it must have its name typed.** Not a checkbox. A
//!   confirmation that can be clicked through is one that gets clicked through, and this is
//!   the only operation in the system that destroys data that was never ours.
//!
//! # Nothing here makes a filesystem except `/var`
//!
//! The ESP, `/usr` and the verity tree are copied **byte for byte** from the running disk,
//! which is possible because both disks have the same layout — the sizes come from the same
//! frozen constants (ADR-0003). That is not only simpler than rebuilding them: it means no
//! `mkfs.vfat` and therefore no new package in the image, which matters because "a program
//! in the image is not a program that can do the job" has cost this project three evenings.
//! `/var` is the exception and `mkfs.xfs` is already there.
//!
//! # What has run
//!
//! **Nothing has been installed.** The reading and the refusals are exercised against
//! fixtures and against the reference laptop; no disk has been written.

use std::io;
use std::path::{Path, PathBuf};

use plexos_gpu::env::Environment;
use plexos_types::gpt;

/// Where the kernel publishes one directory per block device.
const SYS_BLOCK: &str = "/sys/block";

/// A disk that could be installed onto, and everything a person needs to recognise it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Candidate {
    /// Kernel name, e.g. `nvme0n1`.
    pub name: String,
    /// What the drive calls itself, e.g. `KINGSTON SA2000M8500G`.
    pub model: String,
    /// Capacity in bytes.
    pub bytes: u64,
    /// Logical sector size.
    pub sector_size: u64,
    /// Whether the enclosure says it is removable.
    ///
    /// Reported and never *relied* on. It is a property of the enclosure: an internal SD
    /// reader says yes and a USB-attached system disk says yes, and neither answers the
    /// question anybody is actually asking.
    pub removable: bool,
    /// What was found on it, one line per partition.
    ///
    /// The field that turns `/dev/nvme0n1` into somebody's computer. A person deciding
    /// whether to erase a disk recognises `SYSTEM (vfat)` long before they recognise a
    /// device name.
    pub contents: Vec<String>,
    /// Whether this is the disk the installer itself is running from.
    pub is_source: bool,
    /// Why this disk cannot be installed onto, if it cannot.
    pub refusal: Option<String>,
}

/// Why a disk will not be installed onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It is the disk MediaLith is running from.
    IsSource(String),
    /// Which disk this system is running from could not be established, so no disk can be
    /// ruled out and therefore none can be offered.
    ///
    /// Not a property of the disk it is attached to. It is attached to *every* disk,
    /// because the thing that is unknown is which of them must be protected.
    SourceUnknown,
    /// There is no such disk.
    Unknown(String),
    /// It cannot hold the layout.
    TooSmall(String),
    /// The confirmation did not match.
    NotConfirmed {
        /// The disk that was asked for.
        wanted: String,
        /// What was typed instead.
        typed: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IsSource(name) => write!(
                f,
                "{name} is the disk MediaLith is running from, so it cannot be installed \
                 onto. Remedy: choose the disk you want MediaLith to live on. This one is \
                 the installer, and erasing it half way through would leave a machine \
                 with neither system."
            ),
            Self::SourceUnknown => write!(
                f,
                "MediaLith cannot identify the disk it booted from, so no disk can be \
                 offered: the one that must be protected is the one that cannot be named. \
                 Remedy: none from the console. This disk is not refused for anything \
                 about itself, and installing onto the wrong one erases the running \
                 system."
            ),
            Self::Unknown(name) => write!(
                f,
                "there is no disk called {name} on this machine. Remedy: choose one from \
                 the list; it is read from the kernel each time, so a drive plugged in \
                 after the page loaded appears when it is reloaded."
            ),
            Self::TooSmall(why) => write!(f, "{why}"),
            Self::NotConfirmed { wanted, typed } => write!(
                f,
                "this would erase everything on {wanted}, and the confirmation said \
                 {typed:?}. Remedy: type {wanted} exactly. The confirmation is typed \
                 rather than clicked because this is the one thing MediaLith does that \
                 destroys data which was never its own."
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Every disk on this machine, with the one MediaLith is running from marked.
///
/// Partitions, device-mapper devices and loop devices are not disks and are skipped. What
/// remains is what somebody could install onto, whether or not they should.
///
/// # Errors
/// If `/sys/block` cannot be read at all. A single unreadable disk is skipped rather than
/// failing the list: a machine with one odd device must still be installable.
pub fn candidates(env: &impl Environment, source: Option<&str>) -> io::Result<Vec<Candidate>> {
    let mut disks = Vec::new();

    for entry in env.list_dir(Path::new(SYS_BLOCK))? {
        let Some(name) = entry
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if !is_disk(&name) {
            continue;
        }

        let Some(mut disk) = describe(env, &name) else {
            continue;
        };
        disk.is_source = source.is_some_and(|s| s == disk.name);
        // `source` being `None` is the case this whole module's safety turns on, and it is
        // handled here rather than left to callers because there is exactly one safe
        // reading of it and it is not the one that falls out naturally. With no source
        // known, `is_source` is false for every disk, `refusal_for` finds nothing to say
        // about any of them, and the list comes back describing a machine where every disk
        // is free to erase -- which is "nothing is excluded" standing in for "I do not
        // know", the two values this project has already written down as the same value
        // with opposite meanings. `POST /api/install` refused this case from the day it was
        // written; `GET` did not, so the page offered disks the POST would then refuse.
        disk.refusal = match source {
            Some(_) => refusal_for(&disk).map(|r| r.to_string()),
            None => Some(Refusal::SourceUnknown.to_string()),
        };
        disks.push(disk);
    }

    disks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(disks)
}

/// Whether a `/sys/block` entry is a whole disk worth offering.
///
/// Named rather than probed, because the things being excluded are excluded for different
/// reasons and a person reading a list of disks should not see any of them: `loop` and
/// `dm-` are this system's own machinery, `ram` and `zram` are memory, and `sr` is optical.
fn is_disk(name: &str) -> bool {
    !name.starts_with("loop")
        && !name.starts_with("dm-")
        && !name.starts_with("ram")
        && !name.starts_with("zram")
        && !name.starts_with("sr")
        && !name.starts_with("md")
}

/// Reads what sysfs says about one disk.
fn describe(env: &impl Environment, name: &str) -> Option<Candidate> {
    let base = PathBuf::from(SYS_BLOCK).join(name);

    let sectors: u64 = env.read(&base.join("size")).ok()?.trim().parse().ok()?;
    let sector_size: u64 = env
        .read(&base.join("queue/logical_block_size"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(512);

    // A disk of zero sectors is a card reader with no card in it. Offering it produces a
    // confusing failure at the first write rather than an absence anybody expected.
    if sectors == 0 {
        return None;
    }

    let model = env
        .read(&base.join("device/model"))
        .ok()
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "unnamed".to_owned());

    let removable = env
        .read(&base.join("removable"))
        .is_ok_and(|r| r.trim() == "1");

    Some(Candidate {
        name: name.to_owned(),
        model,
        bytes: sectors * sector_size,
        sector_size,
        removable,
        contents: contents_of(env, name),
        is_source: false,
        refusal: None,
    })
}

/// What is already on a disk, one line per partition that says anything about itself.
///
/// Read from sysfs rather than from `blkid`, so that a disk with an unrecognised
/// filesystem still reports its partitions: "four partitions, one of them 400 GiB" is
/// enough for somebody to recognise their own computer, and an empty list because a tool
/// did not know a format is not.
fn contents_of(env: &impl Environment, disk: &str) -> Vec<String> {
    let base = PathBuf::from(SYS_BLOCK).join(disk);
    let Ok(entries) = env.list_dir(&base) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in entries {
        let Some(part) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !part.starts_with(disk) || part == disk {
            continue;
        }
        let Ok(size) = env.read(&entry.join("size")) else {
            continue;
        };
        let Ok(sectors) = size.trim().parse::<u64>() else {
            continue;
        };

        let label = env
            .read(&base.join(part).join("uevent"))
            .ok()
            .and_then(|u| plexos_sys::device::parse_uevent(&u))
            .map(|p| p.partname)
            .filter(|p| !p.is_empty());

        found.push(match label {
            Some(label) => format!("{part}: {} ({label})", human(sectors * 512)),
            None => format!("{part}: {}", human(sectors * 512)),
        });
    }
    found.sort();
    found
}

/// A size somebody can read.
#[must_use]
pub fn human(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        // Integer arithmetic rather than a float: a disk is up to 2^64 bytes and an f64
        // carries 52 bits of mantissa, so the conversion loses precision exactly where the
        // numbers get interesting. Rounded rather than truncated, in u128 so the ×10
        // cannot overflow, because somebody comparing this against what `sgdisk` says
        // about the same disk should see the same number.
        let tenths = (u128::from(bytes) * 10 + u128::from(GIB) / 2) / u128::from(GIB);
        format!("{}.{} GiB", tenths / 10, tenths % 10)
    } else {
        format!("{} MiB", bytes / MIB)
    }
}

/// Why this disk cannot be installed onto, if it cannot.
fn refusal_for(disk: &Candidate) -> Option<Refusal> {
    if disk.is_source {
        return Some(Refusal::IsSource(disk.name.clone()));
    }
    gpt::plan(gpt::Disk {
        sectors: disk.bytes / disk.sector_size,
        sector_size: disk.sector_size,
    })
    .err()
    .map(|error| Refusal::TooSmall(error.to_string()))
}

/// Decides whether an install may proceed, and onto what.
///
/// `typed` is what somebody wrote into the confirmation box. It has to be the disk's name,
/// exactly — see the module documentation for why it is typed rather than ticked.
///
/// # Errors
/// [`Refusal`], each naming what to do about it.
pub fn vet<'a>(disks: &'a [Candidate], name: &str, typed: &str) -> Result<&'a Candidate, Refusal> {
    let disk = disks
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| Refusal::Unknown(name.to_owned()))?;

    // Before the confirmation, so that somebody who typed the name of the disk they are
    // running from is told *that* rather than being told their typing was wrong.
    if let Some(refusal) = refusal_for(disk) {
        return Err(refusal);
    }

    if typed.trim() != disk.name {
        return Err(Refusal::NotConfirmed {
            wanted: disk.name.clone(),
            typed: typed.trim().to_owned(),
        });
    }

    Ok(disk)
}

/// The device name of a partition on a disk.
///
/// `sda` + 1 is `sda1`; `nvme0n1` + 1 is `nvme0n1p1`. The `p` appears when the disk's name
/// already ends in a digit, because `nvme0n11` would otherwise be the eleventh partition of
/// `nvme0n1` and also the first of `nvme0n11`.
#[must_use]
pub fn partition_name(disk: &str, index: usize) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{index}")
    } else {
        format!("{disk}{index}")
    }
}

/// The partitions of the running system, resolved before anything is written.
///
/// **Resolved first, and this is not a stylistic choice.** Partition labels are not unique
/// across disks: the moment the target's table is written, the machine has two partitions
/// called `esp`, two called `usr_a`, and so on. `plexos_sys::device::by_partlabel` would
/// then return whichever the kernel enumerated first, which could perfectly well be the
/// running system — and the next step copies *onto* what it returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The slot this system booted from, and therefore the one being copied.
    pub slot: plexos_types::Slot,
    /// Device path of the running ESP.
    pub esp: String,
    /// Device path of the running `/usr`.
    pub usr: String,
    /// Device path of its verity tree.
    pub verity: String,
}

impl Source {
    /// Resolves the running system's partitions, on the disk it is running from.
    ///
    /// Scoped to a disk for the reason above *and* one more: a second install, run from an
    /// already-installed machine, would otherwise be able to copy the wrong system.
    ///
    /// # Errors
    /// If any of the three cannot be found, which means this is not a MediaLith disk and
    /// there is nothing to copy.
    pub fn resolve(disk: &str, slot: plexos_types::Slot) -> io::Result<Self> {
        Ok(Self {
            slot,
            esp: plexos_sys::device::by_partlabel_on(disk, plexos_types::partition::LABEL_ESP)?,
            usr: plexos_sys::device::by_partlabel_on(disk, slot.usr_label())?,
            verity: plexos_sys::device::by_partlabel_on(disk, slot.verity_label())?,
        })
    }
}

/// The device-mapper name `plexos-init` gives the verified `/usr`.
///
/// Repeated here rather than depended on: `plexosd` does not link `plexos-init`, and the
/// two are the same artefact only by convention. A test pins them together.
const VERITY_MAPPER_NAME: &str = "plexos-usr";

/// The disk this system is running from.
///
/// # Why not the partition label
///
/// That was the first implementation and it was wrong, on hardware, within a minute of the
/// first successful install. Labels are not unique across disks: the moment a target's
/// table is written the machine has two partitions called `esp`, and
/// `by_partlabel` returns whichever the kernel enumerated first — which was the disk that
/// had just been installed onto. The console then reported that MediaLith was running from the
/// *target*, and would have offered the disk it was actually running from as somewhere to
/// install. That is not a cosmetic error: accepting it erases the running system.
///
/// The verified `/usr` is a device-mapper device, and sysfs lists the partitions behind it
/// under `slaves/`. Those are real device names, they cannot collide, and they are what the
/// kernel is actually reading from.
///
/// `None` means the question could not be answered, and callers must treat that as "refuse
/// every disk" rather than as "exclude nothing".
#[must_use]
pub fn running_disk(env: &impl Environment) -> Option<String> {
    let blocks = Path::new("/sys/class/block");
    for entry in env.list_dir(blocks).ok()? {
        let name = entry.file_name()?.to_str()?;
        if !name.starts_with("dm-") {
            continue;
        }
        if env
            .read(&entry.join("dm/name"))
            .ok()
            .is_none_or(|n| n.trim() != VERITY_MAPPER_NAME)
        {
            continue;
        }

        // Several: the verity target reads both the data partition and its hash tree, and
        // they are on the same disk by construction. Any of them answers the question.
        let slave = env
            .list_dir(&entry.join("slaves"))
            .ok()?
            .into_iter()
            .next()?;
        let partition = slave.file_name()?.to_str()?;
        return Some(disk_of(partition));
    }
    None
}

/// The whole disk a partition belongs to: `sda4` is `sda`, `nvme0n1p2` is `nvme0n1`.
///
/// **Takes a partition name.** A disk name cannot be distinguished from a partition name
/// by looking at it — `nvme0n1` is a disk and `sda1` is a partition, and both end in a
/// digit — so handing this a disk gives nonsense. Every caller reads its input from a
/// sysfs `slaves` directory, which contains partitions and nothing else.
#[must_use]
pub fn disk_of(partition: &str) -> String {
    let stem = partition.trim_end_matches(|c: char| c.is_ascii_digit());
    // Only for names that end in a digit before the `p`, which is what distinguishes
    // `nvme0n1p2` (a partition) from a disk that merely ends in `p`.
    if stem.ends_with('p') && stem[..stem.len() - 1].ends_with(|c: char| c.is_ascii_digit()) {
        return stem[..stem.len() - 1].to_owned();
    }
    stem.to_owned()
}

/// What an install is doing, for the console to show.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Nothing has been asked for.
    Idle,
    /// Writing the partition table.
    Partitioning,
    /// Copying the system across.
    Copying,
    /// Making `/var`.
    Formatting,
    /// Installed.
    Done,
    /// Gave up.
    Failed,
}

/// Draws bytes that have to differ between two installed disks.
fn entropy(bytes: usize) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open("/dev/urandom")?;
    let mut out = vec![0u8; bytes];
    file.read_exact(&mut out)?;
    Ok(out)
}

/// A disk GUID and one per partition, drawn from the kernel.
///
/// Fresh every install, deliberately. Two disks carrying the same GUIDs are two disks that
/// tools, and eventually a bootloader, cannot tell apart — and an installer is the one
/// thing guaranteed to produce more than one of them.
fn identity(partitions: usize) -> io::Result<gpt::Identity> {
    let raw = entropy(16 * (partitions + 1))?;
    let at = |i: usize| -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(&raw[i * 16..(i + 1) * 16]);
        // Version 4, variant 1: what every tool expects to see in a GUID it did not make.
        out[6] = (out[6] & 0x0F) | 0x40;
        out[8] = (out[8] & 0x3F) | 0x80;
        out
    };
    Ok(gpt::Identity {
        disk: at(0),
        partitions: (1..=partitions).map(at).collect(),
    })
}

/// Writes the partition table, copies the running system across, and makes `/var`.
///
/// The disk must have come from [`vet`], which is the only thing that decides an install
/// may happen. This function does not re-ask: it is handed a decision and carries it out.
///
/// # Ordering, and why it is this order
///
/// The table first, because everything after it addresses partitions that do not exist
/// until it is written. Then the copies, largest last so that a disk which turns out to be
/// failing does so before an hour of writing. Then `/var`, which is the only filesystem
/// made here and the only step that can be repeated safely.
///
/// # Errors
/// Anything that stops the install. The target disk is left partly written — there is no
/// undo, which is what the typed confirmation was for.
pub fn install(
    target: &Candidate,
    source: &Source,
    log: &mut dyn FnMut(Phase, &str),
) -> io::Result<()> {
    let disk = gpt::Disk {
        sectors: target.bytes / target.sector_size,
        sector_size: target.sector_size,
    };
    let device = format!("/dev/{}", target.name);

    log(
        Phase::Partitioning,
        &format!(
            "writing a partition table to {device} ({}), erasing everything on it",
            human(target.bytes)
        ),
    );

    let identity = identity(plexos_types::partition::LAYOUT_X86_64.len())?;
    let regions = gpt::table(disk, &identity)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    write_regions(&device, disk.sector_size, &regions)?;

    // The kernel is holding the old table. Without this, the partition devices below are
    // either absent or -- worse -- the *previous* occupant's, at the previous offsets.
    reread(&device, log)?;

    // Index by position in the layout, never by label: the target's labels are now
    // duplicates of the running system's, and resolving one by name could return this
    // machine's own disk. See `Source`.
    let name_of = |index: usize| format!("/dev/{}", partition_name(&target.name, index + 1));
    let (esp, usr, verity, var) = (
        name_of(0),
        name_of(slot_index(source.slot).0),
        name_of(slot_index(source.slot).1),
        name_of(5),
    );

    for (what, from, to) in [
        ("the verity tree", source.verity.clone(), verity),
        ("the boot partition", source.esp.clone(), esp),
        ("the system", source.usr.clone(), usr),
    ] {
        log(Phase::Copying, &format!("copying {what}"));
        copy_partition(&from, &to)?;
        log(Phase::Copying, &format!("{what} copied and verified"));
    }

    log(Phase::Formatting, "making an empty /var");
    make_var(&var)?;

    log(
        Phase::Done,
        &format!(
            "installed to {device}. Remove the USB stick and restart: the firmware will \
             find the boot loader on this disk. /var is empty, so the machine comes up as \
             a fresh appliance and the console will ask to be claimed again."
        ),
    );
    Ok(())
}

/// Which layout entries hold a slot's system and verity tree.
const fn slot_index(slot: plexos_types::Slot) -> (usize, usize) {
    match slot {
        plexos_types::Slot::A => (1, 2),
        plexos_types::Slot::B => (3, 4),
    }
}

/// Writes the regions of a partition table at their sector offsets.
fn write_regions(device: &str, sector_size: u64, regions: &[gpt::Region]) -> io::Result<()> {
    use std::io::{Seek as _, SeekFrom, Write as _};

    let mut disk = std::fs::OpenOptions::new().write(true).open(device)?;
    for region in regions {
        disk.seek(SeekFrom::Start(region.lba * sector_size))?;
        disk.write_all(&region.bytes)?;
    }
    disk.sync_all()
}

/// Asks the kernel to read the new table.
fn reread(device: &str, log: &mut dyn FnMut(Phase, &str)) -> io::Result<()> {
    let partprobe =
        plexos_plex::tools::resolve("partprobe", &|p: &Path| p.exists()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "partprobe is not in this image, so the kernel cannot be told about the \
                 new partitions. The table is written; a restart would find them.",
            )
        })?;

    let output = std::process::Command::new(partprobe).arg(device).output()?;
    if !output.status.success() {
        log(
            Phase::Partitioning,
            &format!(
                "partprobe complained: {}. Continuing -- the table is on the disk and the \
                 partitions may still appear.",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        );
    }

    // Settling, rather than trusting partprobe to have finished. There is no udev here to
    // wait on, and a device node that is not there yet fails an open with ENOENT -- which
    // reads as a missing partition rather than as an early one.
    for _ in 0..50 {
        if std::path::Path::new(device).exists() {
            std::thread::sleep(std::time::Duration::from_millis(100));
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

/// Copies one partition onto another and proves the copy arrived.
///
/// Both are block devices of the same size, because both layouts come from the same frozen
/// constants. The digests are taken afterwards over both devices: a copy that silently
/// short-wrote is a system that fails dm-verity on its first boot, days later, looking
/// like a corrupt image rather than like a bad install.
fn copy_partition(from: &str, to: &str) -> io::Result<()> {
    use std::io::Write as _;

    let mut source = std::fs::File::open(from)?;
    let mut target = std::fs::OpenOptions::new().write(true).open(to)?;

    let mut buffer = vec![0u8; 4 * 1024 * 1024];
    loop {
        let read = std::io::Read::read(&mut source, &mut buffer)?;
        if read == 0 {
            break;
        }
        target.write_all(&buffer[..read])?;
    }
    target.sync_all()?;
    drop(target);

    let before = plexos_update::write::digest_of_file(Path::new(from))?;
    let after = plexos_update::write::digest_of_file(Path::new(to))?;
    if before != after {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{to} does not match {from} after copying ({after} against {before}). The \
                 disk did not store what it was given, which usually means it is failing. \
                 Remedy: try another disk; nothing on this machine was changed."
            ),
        ));
    }
    Ok(())
}

/// Makes the empty `/var` a fresh appliance boots into.
fn make_var(device: &str) -> io::Result<()> {
    let mkfs =
        plexos_plex::tools::resolve("mkfs.xfs", &|p: &Path| p.exists()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "mkfs.xfs is not in this image, so /var cannot be made. Remedy: this is a \
             build fault rather than anything about the disk -- BR2_PACKAGE_XFSPROGS.",
            )
        })?;

    // -f because the partition may hold a filesystem from whatever was on this disk
    // before, and refusing would mean an installer that works only on blank disks.
    let output = std::process::Command::new(mkfs)
        .args(["-f", "-L", "var"])
        .arg(device)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "could not make /var on {device}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// The one install this daemon will run, and how far it has got.
///
/// Shaped like [`crate::update::Job`] and [`crate::provision::Job`] for the same reason:
/// writing a disk takes minutes and a request cannot be held open for it. One at a time,
/// because two installs to the same disk is not a thing anybody wants and two to different
/// disks is not a thing this reports usefully.
#[derive(Debug, Default)]
pub struct Job {
    state: std::sync::Mutex<Progress>,
}

/// What `GET /api/install` reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Progress {
    /// Where the run is.
    pub phase: Phase,
    /// One line describing what is happening now.
    pub detail: String,
    /// Why it failed, if it did.
    pub error: Option<String>,
    /// The disks on this machine, refreshed on every read.
    pub disks: Vec<Candidate>,
    /// The disk MediaLith is running from, if that could be established.
    ///
    /// `None` is why every disk in `disks` carries a refusal, and the page needs to be able
    /// to tell that apart from a machine with no usable disks: both show nothing to install
    /// onto, and they call for opposite sentences. Reported as well as acted on, because a
    /// person told "no disk can be used" deserves to know it is not about their disks.
    pub source: Option<String>,
    /// Everything said so far.
    pub log: Vec<String>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            detail: "nothing has been installed".to_owned(),
            error: None,
            disks: Vec::new(),
            source: None,
            log: Vec::new(),
        }
    }
}

impl Job {
    /// A job that has installed nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current state, with the disk list read fresh.
    #[must_use]
    pub fn snapshot(&self, env: &impl Environment, source: Option<&str>) -> Progress {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Read on every request rather than cached: a disk plugged in while somebody is
        // looking at the page should appear, and a cached list is one that offers a disk
        // that has been unplugged.
        state.disks = candidates(env, source).unwrap_or_default();
        // Set here rather than at `begin`, so it is refreshed with the disks it explains
        // and cannot be left over from a snapshot taken under different conditions.
        state.source = source.map(str::to_owned);
        state.clone()
    }

    /// Claims the job, if no install holds it.
    pub fn begin(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            state.phase,
            Phase::Partitioning | Phase::Copying | Phase::Formatting
        ) {
            return false;
        }
        *state = Progress {
            phase: Phase::Partitioning,
            detail: "starting".to_owned(),
            ..Progress::default()
        };
        true
    }

    /// Records a step.
    pub fn step(&self, phase: Phase, detail: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.phase = phase;
        detail.clone_into(&mut state.detail);
        if state.log.len() >= 200 {
            state.log.remove(0);
        }
        state.log.push(detail.to_owned());
    }

    /// Records the outcome.
    pub fn finish(&self, outcome: Result<(), String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match outcome {
            Ok(()) => state.phase = Phase::Done,
            Err(error) => {
                state.phase = Phase::Failed;
                "the install failed".clone_into(&mut state.detail);
                state.log.push(error.clone());
                state.error = Some(error);
            }
        }
    }
}

/// Runs an install in a new thread, reporting into `job`.
///
/// The disk has already been vetted by the caller: this is handed a decision, not asked to
/// make one.
pub fn spawn(job: &std::sync::Arc<Job>, target: Candidate, source: Source) {
    let job = std::sync::Arc::clone(job);
    std::thread::spawn(move || {
        let outcome = install(&target, &source, &mut |phase, detail| {
            println!("plexosd: install: {detail}");
            job.step(phase, detail);
        });
        job.finish(outcome.map_err(|error| error.to_string()));
    });
}

/// The disk and confirmation a request asked for.
#[must_use]
pub fn request_in(body: &[u8]) -> (String, String) {
    let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let field = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    (field("disk"), field("confirm"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_gpu::env::Fixture;

    /// A machine shaped like the reference laptop: an internal `NVMe` with Windows on it,
    /// and the USB stick MediaLith booted from.
    ///
    /// Built from the sizes and models read off that machine rather than invented, because
    /// a fixture somebody imagined is a test that agrees with the code and not with the
    /// hardware — which this project has already paid for once, in a resolver parser.
    fn laptop() -> Fixture {
        let mut fixture = Fixture::new()
            .file(format!("{SYS_BLOCK}/nvme0n1/size"), "976773168\n")
            .file(format!("{SYS_BLOCK}/nvme0n1/removable"), "0\n")
            .file(
                format!("{SYS_BLOCK}/nvme0n1/device/model"),
                "KINGSTON SA2000M8500G\n",
            )
            .file(
                format!("{SYS_BLOCK}/nvme0n1/queue/logical_block_size"),
                "512\n",
            )
            .file(format!("{SYS_BLOCK}/sda/size"), "30703616\n")
            .file(format!("{SYS_BLOCK}/sda/removable"), "1\n")
            .file(format!("{SYS_BLOCK}/sda/device/model"), "DT Rubber 3.0\n")
            .file(format!("{SYS_BLOCK}/sda/queue/logical_block_size"), "512\n")
            .file(format!("{SYS_BLOCK}/sda/sda1/size"), "1048576\n")
            .file(
                format!("{SYS_BLOCK}/sda/sda1/uevent"),
                "DEVNAME=sda1\nPARTNAME=esp\n",
            )
            // This system's own machinery, which must never be offered as somewhere to
            // install: loop0 is the mounted Plex app image, dm-0 the verified /usr.
            .file(format!("{SYS_BLOCK}/loop0/size"), "137920\n")
            .file(format!("{SYS_BLOCK}/dm-0/size"), "152840\n");

        for (part, sectors, label) in [
            ("nvme0n1p1", "204800", "SYSTEM"),
            ("nvme0n1p2", "32768", ""),
            ("nvme0n1p3", "976533504", ""),
        ] {
            fixture = fixture
                .file(
                    format!("{SYS_BLOCK}/nvme0n1/{part}/size"),
                    format!("{sectors}\n"),
                )
                .file(
                    format!("{SYS_BLOCK}/nvme0n1/{part}/uevent"),
                    format!("DEVNAME={part}\nPARTNAME={label}\n"),
                );
        }
        fixture
    }

    #[test]
    fn the_disk_the_installer_runs_from_is_refused_however_it_is_attached() {
        // The `removable` flag is not the test and must not be: it is a property of the
        // enclosure, true for an internal card reader and true for a USB disk somebody
        // runs their whole system from.
        let disks = candidates(&laptop(), Some("sda")).unwrap();
        let stick = disks.iter().find(|d| d.name == "sda").unwrap();

        assert!(stick.is_source);
        let refusal = stick.refusal.as_deref().expect("refused");
        assert!(refusal.contains("running from"), "{refusal}");
        assert!(refusal.contains("Remedy:"), "{refusal}");

        assert!(matches!(
            vet(&disks, "sda", "sda"),
            Err(Refusal::IsSource(_))
        ));
    }

    #[test]
    fn an_unknown_running_disk_refuses_every_disk_rather_than_excluding_none() {
        // The defect this closes: `GET /api/install` passed `None` straight through, so
        // `is_source` was false everywhere, no disk collected a refusal, and the page drew
        // a radio button and an install button beside every disk on the machine -- while
        // `POST` refused the request outright. The backend was right and the page invited
        // somebody to make a request that could not succeed, which is the worst of both:
        // it reads as a bug in the appliance, and the one time it is not is the time
        // somebody finds a way to force it through.
        let disks = candidates(&laptop(), None).unwrap();
        assert!(!disks.is_empty(), "the fixture has disks to refuse");

        for disk in &disks {
            let refusal = disk
                .refusal
                .as_deref()
                .unwrap_or_else(|| panic!("{} was offered with no source known", disk.name));
            assert!(
                refusal.contains("cannot identify the disk it booted from"),
                "{refusal}"
            );
            assert!(refusal.contains("Remedy:"), "{refusal}");
            // Not about the disk. Somebody reading this must not go looking for a fault in
            // a drive that has nothing wrong with it.
            assert!(
                refusal.contains("not refused for anything about itself"),
                "{refusal}"
            );
        }

        // And the flag is still false, because none of them *is* the source -- which is
        // precisely why the refusal cannot be carried by `is_source`.
        assert!(disks.iter().all(|d| !d.is_source));
    }

    #[test]
    fn the_snapshot_says_which_disk_it_booted_from_so_the_page_can_tell_the_states_apart() {
        // "Every disk is refused" is reached by two roads: nothing here is big enough, and
        // the boot disk could not be named. They need opposite sentences, and the disk list
        // alone cannot distinguish them.
        let job = Job::new();

        let known = job.snapshot(&laptop(), Some("sda"));
        assert_eq!(known.source.as_deref(), Some("sda"));
        assert!(
            known.disks.iter().any(|d| d.refusal.is_none()),
            "with the source known, something is installable"
        );

        let unknown = job.snapshot(&laptop(), None);
        assert_eq!(unknown.source, None);
        assert!(
            unknown.disks.iter().all(|d| d.refusal.is_some()),
            "with the source unknown, nothing is"
        );
    }

    #[test]
    fn a_disk_is_described_the_way_its_owner_would_recognise_it() {
        // `/dev/nvme0n1` is a device name. "KINGSTON, 465.8 GiB, nvme0n1p1: 100.0 MiB
        // (SYSTEM)" is somebody's computer, and this is the one decision where being able
        // to tell those apart is the whole safety story.
        let disks = candidates(&laptop(), Some("sda")).unwrap();
        let internal = disks.iter().find(|d| d.name == "nvme0n1").unwrap();

        assert_eq!(internal.model, "KINGSTON SA2000M8500G");
        assert_eq!(human(internal.bytes), "465.8 GiB");
        assert!(!internal.removable);
        assert_eq!(internal.contents.len(), 3, "{:?}", internal.contents);
        assert!(
            internal.contents.iter().any(|c| c.contains("SYSTEM")),
            "the partition that says this is a Windows disk: {:?}",
            internal.contents
        );
    }

    #[test]
    fn this_systems_own_machinery_is_not_offered_as_a_disk() {
        // loop0 is the mounted Plex app image and dm-0 is the verified /usr. Offering
        // either would be offering to install MediaLith onto MediaLith.
        let names: Vec<String> = candidates(&laptop(), Some("sda"))
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["nvme0n1".to_owned(), "sda".to_owned()]);
    }

    #[test]
    fn installing_needs_the_disks_name_typed_and_not_a_tick() {
        let disks = candidates(&laptop(), Some("sda")).unwrap();

        match vet(&disks, "nvme0n1", "yes") {
            Err(Refusal::NotConfirmed { wanted, typed }) => {
                assert_eq!(wanted, "nvme0n1");
                assert_eq!(typed, "yes");
            }
            other => panic!("a tick must not be enough: {other:?}"),
        }

        // And the right name, with the whitespace a browser adds, is accepted.
        assert_eq!(vet(&disks, "nvme0n1", " nvme0n1 ").unwrap().name, "nvme0n1");
    }

    #[test]
    fn a_disk_that_is_not_there_is_not_installed_onto() {
        let disks = candidates(&laptop(), Some("sda")).unwrap();
        let error = vet(&disks, "sdz", "sdz").unwrap_err();
        assert!(matches!(error, Refusal::Unknown(_)));
        assert!(error.to_string().contains("Remedy:"));
    }

    #[test]
    fn being_the_source_is_reported_before_the_confirmation_is_judged() {
        // Somebody who typed the name of the disk they are running from has made a
        // mistake about *which disk*, and telling them their typing was wrong sends them
        // to type it again more carefully.
        let disks = candidates(&laptop(), Some("sda")).unwrap();
        assert!(matches!(
            vet(&disks, "sda", "something else"),
            Err(Refusal::IsSource(_))
        ));
    }

    #[test]
    fn a_disk_too_small_for_the_layout_is_refused_with_both_numbers() {
        let fixture = laptop().file(format!("{SYS_BLOCK}/sda/size"), "2097152\n"); // 1 GiB

        let disks = candidates(&fixture, Some("nvme0n1")).unwrap();
        let small = disks.iter().find(|d| d.name == "sda").unwrap();
        let refusal = small.refusal.as_deref().expect("refused");
        assert!(refusal.contains("Remedy:"), "{refusal}");
        assert!(refusal.contains("larger disk"), "{refusal}");
    }

    #[test]
    fn a_card_reader_with_no_card_is_not_a_disk() {
        // Zero sectors. Offering it produces a confusing failure at the first write
        // rather than an absence anybody expected.
        let fixture = laptop().file(format!("{SYS_BLOCK}/sda/size"), "0\n");
        let names: Vec<String> = candidates(&fixture, None)
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["nvme0n1".to_owned()]);
    }

    #[test]
    fn a_partition_is_named_the_way_the_kernel_names_it() {
        // nvme0n11 would otherwise be both the eleventh partition of nvme0n1 and the first
        // of nvme0n11, and this installer addresses partitions by name because their
        // labels are no longer unique once the target's table is written.
        assert_eq!(partition_name("sda", 1), "sda1");
        assert_eq!(partition_name("sdb", 6), "sdb6");
        assert_eq!(partition_name("nvme0n1", 1), "nvme0n1p1");
        assert_eq!(partition_name("nvme0n1", 6), "nvme0n1p6");
        assert_eq!(partition_name("mmcblk0", 2), "mmcblk0p2");
    }

    #[test]
    fn each_slot_copies_into_its_own_pair_of_partitions() {
        // The installer writes the slot it is running from, so a machine installed from a
        // slot-B stick boots slot B. Getting this wrong copies the system into the
        // partition the UKI does not name, and dm-verity refuses it on the first boot.
        let layout = plexos_types::partition::LAYOUT_X86_64;
        let (usr_a, verity_a) = slot_index(plexos_types::Slot::A);
        let (usr_b, verity_b) = slot_index(plexos_types::Slot::B);

        assert_eq!(layout[usr_a].label, "usr_a");
        assert_eq!(layout[verity_a].label, "usr_a_hash");
        assert_eq!(layout[usr_b].label, "usr_b");
        assert_eq!(layout[verity_b].label, "usr_b_hash");
        assert_eq!(layout[0].label, plexos_types::partition::LABEL_ESP);
        assert_eq!(layout[5].label, plexos_types::partition::LABEL_VAR);
    }

    #[test]
    fn every_installed_disk_gets_its_own_identifiers() {
        // Two disks with the same GUIDs are two disks nothing can tell apart, and an
        // installer is the one thing certain to produce more than one.
        let first = identity(6).expect("entropy");
        let second = identity(6).expect("entropy");

        assert_ne!(first.disk, second.disk);
        assert_eq!(first.partitions.len(), 6);
        for (a, b) in first.partitions.iter().zip(&second.partitions) {
            assert_ne!(a, b);
        }
        // Version 4, variant 1, so tools render them as GUIDs rather than as something
        // they report as malformed.
        assert_eq!(first.disk[6] & 0xF0, 0x40);
        assert_eq!(first.disk[8] & 0xC0, 0x80);
    }

    #[test]
    fn a_request_that_says_nothing_installs_nothing() {
        // The safe reading of an unintelligible body is the one that changes no disk.
        assert_eq!(request_in(b"{}"), (String::new(), String::new()));
        assert_eq!(request_in(b""), (String::new(), String::new()));
        assert_eq!(
            request_in(br#"{"disk":"nvme0n1","confirm":"nvme0n1"}"#),
            ("nvme0n1".to_owned(), "nvme0n1".to_owned())
        );

        // And an empty confirmation never matches a disk, so the default is a refusal.
        let disks = candidates(&laptop(), Some("sda")).unwrap();
        assert!(vet(&disks, "nvme0n1", "").is_err());
    }

    #[test]
    fn only_one_install_may_hold_the_job() {
        let job = Job::new();
        assert!(job.begin());
        assert!(
            !job.begin(),
            "a second install must not start behind the first"
        );
        job.finish(Ok(()));
        assert!(job.begin(), "and a finished one releases it");
    }

    #[test]
    fn a_fresh_job_reports_the_disks_and_has_installed_nothing() {
        let job = Job::new();
        let progress = job.snapshot(&laptop(), Some("sda"));
        assert_eq!(progress.phase, Phase::Idle);
        assert!(progress.error.is_none());
        assert_eq!(progress.disks.len(), 2);
        assert!(progress.disks.iter().any(|d| d.is_source));
    }

    #[test]
    fn the_running_disk_is_found_behind_the_verified_usr_and_not_by_label() {
        // Found on hardware within a minute of the first successful install. The first
        // implementation resolved the ESP by partition label -- and the moment a target's
        // table is written there are two partitions called `esp`, so it returned the disk
        // that had just been installed onto. The console then reported MediaLith as running
        // from the target, and would have offered the disk it was really running from.
        // Accepting that erases the running system.
        let fixture = laptop()
            .file("/sys/class/block/dm-0/dm/name", "plexos-usr\n")
            .file("/sys/class/block/dm-0/slaves/sda4/partition", "4\n")
            .file("/sys/class/block/dm-0/slaves/sda5/partition", "5\n")
            // The duplicate label that broke it, present exactly as it would be.
            .file(
                format!("{SYS_BLOCK}/nvme0n1/nvme0n1p1/uevent"),
                "DEVNAME=nvme0n1p1\nPARTNAME=esp\n",
            );

        assert_eq!(running_disk(&fixture).as_deref(), Some("sda"));
    }

    #[test]
    fn a_machine_with_no_verified_usr_answers_nothing_rather_than_guessing() {
        // The caller must treat this as "refuse every disk". Guessing here is guessing
        // about which disk to erase.
        assert_eq!(running_disk(&laptop()), None);
    }

    #[test]
    fn a_partition_name_resolves_to_the_disk_it_is_on() {
        assert_eq!(disk_of("sda4"), "sda");
        assert_eq!(disk_of("nvme0n1p2"), "nvme0n1");
        assert_eq!(disk_of("mmcblk0p1"), "mmcblk0");

        // The inverse of the name the installer writes to, which is the property that
        // matters: the two functions are used at opposite ends of the same operation.
        for disk in ["sda", "nvme0n1", "mmcblk0"] {
            for index in 1..=6 {
                assert_eq!(disk_of(&partition_name(disk, index)), disk);
            }
        }

        // A disk name is not something this can be asked about, and pretending otherwise
        // would hide that: `nvme0n1` is a disk, `sda1` is a partition, and nothing in the
        // strings tells them apart.
    }

    #[test]
    fn the_mapper_name_is_the_one_pid_one_creates() {
        // plexosd does not link plexos-init, so these agree by convention. If they ever
        // stop agreeing, running_disk returns None and every install is refused -- which
        // is the safe direction and an unusable one.
        assert_eq!(VERITY_MAPPER_NAME, "plexos-usr");
    }
}
