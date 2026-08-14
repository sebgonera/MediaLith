//! A media library on a disk in this machine, rather than on a server.
//!
//! [`crate::shares`] assumes the library lives on a NAS, because that is where the
//! library lived on the machine this was built for. That assumption is not the common
//! case and never was. Somebody who boots MediaLith from a USB stick on the computer they
//! already own has their films **on that computer** — on the Windows partition of its
//! internal disk, or on an external drive they plug in. Both are inches away, both
//! enumerate, and until this module existed neither could be played.
//!
//! # Read-only, and the one place that turns out to be a feature
//!
//! Everything here mounts `ro,nosuid,nodev,noexec`, for the reasons [`crate::shares`]
//! gives: a library is read, and an appliance that could delete somebody's films on a bug
//! is a worse appliance. On a disk that is somebody's *only* copy — which an internal
//! Windows disk always is — that stops being a preference.
//!
//! It also happens to be what makes the motivating case work at all. A Windows shut down
//! by Fast Startup, or hibernated, leaves `VOLUME_FLAG_DIRTY` set and an unreplayed log,
//! and `ntfs3` refuses both — but **both refusals are guarded by `!ro`**
//! (`fs/ntfs3/super.c:1367,1373`, read in the 6.19.14 tree rather than recalled). So a
//! read-only mount of a hibernated Windows partition succeeds, and nobody has to be told
//! to go and shut Windows down properly before their media server will work.
//!
//! # Ownership, which these filesystems do not have
//!
//! NTFS, exFAT and FAT have no Unix owner. `ntfs3` fills one in from the mounting
//! process: `opts->fs_uid = current_uid()` and `fs_fmask_inv = ~current_umask()`
//! (`super.c:1804`), and exFAT and FAT do the same. `plexosd` is root, so without saying
//! otherwise the result depends on **the daemon's umask** — and Plex runs as uid 900.
//!
//! That is the render-node defect exactly: every layer above reports success, the mount
//! is there, the files are listed by anything probing as root, and only Plex finds an
//! empty library. So [`options_for`] passes `uid`, `gid`, `fmask` and `dmask` explicitly
//! and the result does not depend on who mounted it or what their umask was.
//!
//! The Unix filesystems are the other way round and cannot be fixed here: ext4 and XFS
//! carry real ownership, so a drive written by another Linux machine may be unreadable to
//! uid 900 whatever this mounts it with. [`Volume::readable_by_plex`] asks the question
//! instead of assuming the answer, because a library that silently scans as empty is the
//! worst of the three outcomes.
//!
//! # Identity is the partition, not the device name
//!
//! `/dev/sdb1` is what the kernel called it during one boot. Recording that would give a
//! library that comes back pointing at a different drive the first time somebody plugs
//! two things in — which is the label-ambiguity defect this project has already paid for
//! once, in the place where it costs a partition write.
//!
//! So a library is recorded by `PARTUUID`, which the kernel puts in every partition's
//! `uevent` and which is unique across disks. It exists for MBR too:
//! `block/partitions/msdos.c:113` synthesises `%08x-%02x` from the disk signature and the
//! partition number, so a USB drive partitioned by Windows is covered as well as a GPT
//! one. A volume with no `PARTUUID` — a filesystem written straight to a whole disk with
//! no partition table — can be browsed and cannot be *remembered*, and says so.
//!
//! # Bind mounts, and a flag argument that does nothing
//!
//! A person picks a folder, not a volume: `D:\Films` and not the whole of somebody's
//! Windows install. So the volume is mounted under [`SCAN_ROOT`] and the chosen folder is
//! bind-mounted from there to `/var/media/<name>`, which is where Plex is granted to
//! look.
//!
//! `mount(2)` with `MS_BIND` **ignores every other flag** — `fs/namespace.c:4025` hands
//! straight off to `do_loopback`, and the new mount copies the source's `mnt_flags`
//! verbatim (`fs/namespace.c:1256`). The bind is therefore read-only because the volume
//! under it is, and passing `ro` to it would be a string that reads like it is doing the
//! work and is discarded. [`bind`] passes `bind` alone for that reason: the next person to
//! add `rw` there should find out from the code that it would change nothing.
//!
//! # Mounted before Plex starts
//!
//! Same doctrine as [`crate::shares`], and for the same unanswered question about whether
//! a Landlock rule on `/var/media` reaches a filesystem mounted underneath it afterwards.
//! [`mount_all`] runs on the boot path before Plex, and adding one from the console offers
//! to restart Plex rather than hoping.
//!
//! # What has run
//!
//! **Nothing here has mounted anything on a machine.** The pure parts are tested against
//! captured sysfs; mounting needs root, a disk, and a kernel with `CONFIG_NTFS3_FS`, which
//! no built image has yet carried. Delete this notice when it has.

use std::io;
use std::path::{Path, PathBuf};

use plexos_gpu::env::{Environment, System};
use serde::{Deserialize, Serialize};

/// Where the kernel publishes one directory per block device.
const SYS_BLOCK: &str = "/sys/block";

/// Where volumes are mounted while somebody is looking through them.
///
/// Under `/run` because it is a tmpfs the running root already has, and because a mount
/// left behind across a reboot would hold a drive somebody has physically removed.
/// [`crate::media`] uses `/run/plexos/media` for the same reason and for a different
/// purpose; the two are kept apart so that unmounting one cannot disturb the other.
pub const SCAN_ROOT: &str = "/run/plexos/disks";

/// Where the chosen libraries are recorded.
///
/// Under the state root, so a library survives an OS update and a rollback. ADR-0009
/// permits an addition like this: a release that has never heard of the file ignores it.
pub const CONFIG: &str = "/var/lib/plexos/disks.json";

/// Where a library appears for Plex. The same directory network shares use.
pub const ROOT: &str = plexos_types::paths::MEDIA;

/// Filesystems tried, in order.
///
/// The order is not arbitrary and not a guess about popularity. The first three are
/// checked strictly — `ntfs3` compares the boot sector's `system_id` against `"NTFS    "`
/// (`fs/ntfs3/super.c:966`) and exFAT does the equivalent — so they cannot claim a volume
/// that is not theirs. `vfat` is the loose one and goes after them. `ntfs3` leads because
/// the case this module was written for is an internal Windows disk.
///
/// Every name here is a `CONFIG_*` symbol asserted by stage 7b of `post-image-test.sh`. A
/// name the kernel cannot mount produces a failure blamed on somebody's drive.
pub const FILESYSTEMS: [&str; 6] = ["ntfs3", "exfat", "vfat", "ext4", "xfs", "iso9660"];

/// The kernel symbol that has to be built for a filesystem name to mount anything.
///
/// One place, because [`crate::media`] offers an overlapping list for a different purpose
/// and both are the same half of the same decision: a name in either list without the
/// symbol in `linux.fragment` produces `EINVAL`, an error about *arguments* that reads as
/// a broken drive. Both modules' tests assert against this, so adding a filesystem to
/// either one fails here first if the kernel was never asked for it.
///
/// Returns `None` for a name neither module has recorded a symbol for, which is itself the
/// thing worth failing on.
#[must_use]
pub fn kernel_symbol(fstype: &str) -> Option<&'static str> {
    Some(match fstype {
        "ntfs3" => "CONFIG_NTFS3_FS=y",
        "vfat" => "CONFIG_VFAT_FS=y",
        "exfat" => "CONFIG_EXFAT_FS=y",
        "ext4" => "CONFIG_EXT4_FS=y",
        "xfs" => "CONFIG_XFS_FS=y",
        "iso9660" => "CONFIG_ISO9660_FS=y",
        _ => return None,
    })
}

/// Mount options every volume gets, whatever it is.
///
/// Identical to [`crate::shares::FIXED_OPTIONS`] and deliberately so: where the library
/// lives should not change what the appliance is allowed to do to it.
pub const FIXED_OPTIONS: &str = "ro,nosuid,nodev,noexec";

/// The uid a library has to be readable by, which is the uid Plex runs as.
pub const PLEX_UID: u32 = 900;

/// How many entries one directory listing returns.
///
/// A folder of ten thousand episodes is a real thing and a page listing all of them is
/// not useful. The listing says when it has been cut, which is the part that matters:
/// a truncation nobody is told about reads as a directory with fewer files in it than it
/// has.
pub const BROWSE_LIMIT: usize = 400;

/// What counts as something Plex would play, for the count shown beside a folder.
///
/// The point of the count is to tell `Movies` from `System Volume Information` at a
/// glance, so this errs towards recognising things: a folder reported as holding no media
/// when it holds some is a folder somebody skips past.
pub const MEDIA_EXTENSIONS: [&str; 22] = [
    "mkv", "mp4", "avi", "m4v", "mov", "wmv", "mpg", "mpeg", "m2ts", "ts", "iso", "flv", "webm",
    "mp3", "flac", "m4a", "aac", "ogg", "wav", "wma", "opus", "alac",
];

/// Mount options for one filesystem, including the ownership it cannot supply itself.
///
/// The `uid`/`gid`/`fmask`/`dmask` half is only meaningful for the three filesystems with
/// no Unix ownership, and is the difference between a library Plex can read and one it
/// cannot. `fmask=0133` is `r--r--r--`, `dmask=0022` is `r-xr-xr-x` — a directory needs
/// its execute bit to be walked into, which `noexec` on the mount does not take away
/// because `noexec` is about executing files.
///
/// `utf8=1` for FAT only. exFAT and `ntfs3` already default to UTF-8 here
/// (`CONFIG_EXFAT_DEFAULT_IOCHARSET` and `CONFIG_NLS_DEFAULT`, both `"utf8"`), while FAT's
/// default is `CONFIG_FAT_DEFAULT_IOCHARSET="iso8859-1"` — which mangles every non-ASCII
/// filename. `utf8=1` rather than `iocharset=utf8`: the latter makes the filesystem case
/// sensitive and the kernel warns about it (`fs/fat/inode.c:1576`), which is not what
/// anybody wants from a FAT drive.
#[must_use]
pub fn options_for(fstype: &str) -> String {
    let ownership = "uid=0,gid=0,fmask=0133,dmask=0022";
    match fstype {
        "ntfs3" | "exfat" => format!("{FIXED_OPTIONS},{ownership}"),
        "vfat" => format!("{FIXED_OPTIONS},{ownership},utf8=1"),
        // ext4, xfs and iso9660 carry their own ownership. Saying otherwise here would
        // be an option the kernel rejects, which is an EINVAL blamed on the drive.
        _ => FIXED_OPTIONS.to_owned(),
    }
}

/// A partition that might hold a library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Volume {
    /// Kernel name, e.g. `sdb1`, or a whole disk with a filesystem written straight to it.
    pub name: String,
    /// The device node to mount.
    pub device: String,
    /// The whole disk it is on, for grouping on the page.
    pub disk: String,
    /// What the drive calls itself, e.g. `Samsung SSD 860`.
    pub model: String,
    /// Unique across disks, and stable across reboots. `None` for a whole-disk filesystem.
    pub partuuid: Option<String>,
    /// The GPT partition name, when it has one. MBR partitions have none.
    pub label: Option<String>,
    /// Size in bytes.
    pub bytes: u64,
    /// What it turned out to be, once something mounted it.
    pub filesystem: Option<String>,
    /// Where it is mounted for browsing, if it is.
    pub mounted_at: Option<String>,
    /// Whether uid 900 can actually walk into it.
    pub readable_by_plex: Option<bool>,
    /// Why it is not offered, if it is not.
    pub refusal: Option<String>,
}

impl Volume {
    /// Where this volume is mounted while it is being looked through.
    ///
    /// Named by `PARTUUID` rather than by `sdb1`, so that the path a browser is holding
    /// does not come to mean a different drive if the kernel renumbers between two scans.
    #[must_use]
    pub fn scan_point(&self) -> PathBuf {
        let key = self.partuuid.clone().unwrap_or_else(|| self.name.clone());
        Path::new(SCAN_ROOT).join(key)
    }
}

/// Why a volume is not offered as a library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It is on the disk MediaLith is running from.
    RunningDisk(String),
    /// It is one of MediaLith's own partitions on some other disk.
    OwnPartition(String),
    /// Which disk this system runs from could not be established.
    ///
    /// Attached to every volume rather than to one, because what is unknown is which of
    /// them must be protected. The same reasoning as [`crate::install::Refusal::SourceUnknown`].
    SourceUnknown,
    /// Nothing in this kernel could mount it.
    Unmountable {
        /// The device that would not mount.
        device: String,
        /// What the last attempt said.
        cause: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RunningDisk(disk) => write!(
                f,
                "this is on {disk}, the disk MediaLith is running from. Remedy: none is \
                 needed — the appliance's own partitions are not a media library, and \
                 nothing on this disk is what you are looking for."
            ),
            Self::OwnPartition(label) => write!(
                f,
                "this is MediaLith's own {label} partition on another disk — most likely \
                 the stick this machine was installed from. Remedy: none is needed. It \
                 can be mounted and holds nothing to play."
            ),
            Self::SourceUnknown => write!(
                f,
                "MediaLith cannot identify the disk it booted from, so no volume can be \
                 offered: the one that must be left alone is the one that cannot be \
                 named. Remedy: none from the console. This is not something about this \
                 volume."
            ),
            Self::Unmountable { device, cause } => write!(
                f,
                "{device} could not be mounted as any of {}: {cause}. Remedy: if this is \
                 a Windows drive that BitLocker is encrypting, MediaLith cannot read it \
                 and no option here will change that — unlock it in Windows and turn \
                 BitLocker off for that drive, or copy the library somewhere else. If it \
                 is a Linux drive using btrfs or ZFS, this kernel does not build either.",
                FILESYSTEMS.join(", ")
            ),
        }
    }
}

/// A directory somebody is deciding about, with enough beside it to decide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The name alone, for showing.
    pub name: String,
    /// The full path, for going into or choosing.
    pub path: String,
    /// How many things Plex would play are directly in it.
    pub media_here: usize,
    /// How many subdirectories it has.
    pub folders: usize,
}

/// What a directory listing found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    /// The directory this describes.
    pub path: String,
    /// The directory above it, or `None` at the top of a volume.
    ///
    /// Computed here rather than in the browser, because the browser must never be the
    /// thing that decides how far up it may go.
    pub parent: Option<String>,
    /// Subdirectories, sorted.
    pub entries: Vec<Entry>,
    /// Media files directly in this directory.
    pub media_here: usize,
    /// Whether [`BROWSE_LIMIT`] cut the list.
    pub truncated: bool,
}

/// One remembered library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    /// A short name, which is also the directory it appears under in `/var/media`.
    pub name: String,
    /// Which partition it is on. The identifier that survives a reboot.
    pub partuuid: String,
    /// Where inside the volume, relative and possibly empty for the whole of it.
    pub subpath: String,
    /// What mounted it last time.
    ///
    /// A hint for the message when it does not come back, not an instruction: the volume
    /// is probed again on every boot, because a drive that was reformatted should produce
    /// a clear failure rather than a mount attempt with a stale filesystem name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<String>,
}

impl Library {
    /// Where this appears for Plex.
    #[must_use]
    pub fn mount_point(&self) -> PathBuf {
        Path::new(ROOT).join(&self.name)
    }

    /// Whether the name is one that can be a directory under [`ROOT`] and nothing else.
    ///
    /// The same rule as [`crate::shares::Share::has_safe_name`], and refused as a shape
    /// rather than sanitised for the same reason: the name is joined to a path, so a `..`
    /// or a `/` would let whoever can reach the console choose where a mount lands.
    #[must_use]
    pub fn has_safe_name(&self) -> bool {
        !self.name.is_empty()
            && self.name.len() <= 64
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }
}

/// Why a library could not be added or brought back.
#[derive(Debug)]
pub enum Error {
    /// The name is not a plain directory name.
    BadName(String),
    /// A library of that name already exists.
    Duplicate(String),
    /// The chosen path is not on a scanned volume.
    NotOnAVolume(String),
    /// The volume has no identifier that would still mean this drive after a restart.
    ///
    /// Its own variant rather than a [`Error::NotOnAVolume`] carrying a long string: the
    /// path is perfectly good and the drive is right there. What is missing is a
    /// partition table, and that is a different thing to tell somebody.
    Unrememberable {
        /// The volume, by whatever it could be called.
        volume: String,
    },
    /// The partition that library is on is not attached.
    Absent {
        /// Which library.
        name: String,
        /// The partition it wants.
        partuuid: String,
    },
    /// The folder inside the volume is not there any more.
    Vanished {
        /// Which library.
        name: String,
        /// What it was looking for.
        path: String,
    },
    /// A mount failed.
    Mount {
        /// Where it was going.
        target: PathBuf,
        /// Why.
        cause: io::Error,
    },
    /// Nothing in this kernel would mount the volume.
    Unmountable(Refusal),
    /// Reading or writing the record failed.
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(name) => write!(
                f,
                "{name:?} is not a usable name for a library. The name becomes a directory \
                 under {ROOT}, so anything else would let it land somewhere it should not. \
                 Remedy: use letters, digits, dashes and underscores only, and at most 64 \
                 of them — `films` or `Seb_Movies`."
            ),
            Self::Duplicate(name) => write!(
                f,
                "there is already a library called {name:?}. Remedy: pick another name, \
                 or remove the existing one first — two things mounted at the same place \
                 would leave Plex reading whichever won."
            ),
            Self::NotOnAVolume(path) => write!(
                f,
                "{path} is not a folder on a scanned drive. Only paths under {SCAN_ROOT} \
                 can be added this way. Remedy: scan for drives and choose a folder from \
                 the list rather than typing a path."
            ),
            Self::Unrememberable { volume } => write!(
                f,
                "{volume} has no partition table, so there is no identifier that would \
                 still mean this drive after a restart — a device name like /dev/sdb is \
                 whatever the kernel happened to call it during one boot. The folder can \
                 be looked through now and cannot be remembered. Remedy: partition the \
                 drive on another computer — one partition covering the whole of it is \
                 enough — or copy the library onto a drive that is already partitioned, \
                 which is every drive Windows or macOS has ever formatted."
            ),
            Self::Absent { name, partuuid } => write!(
                f,
                "the drive holding {name} is not attached: no partition on this machine \
                 has PARTUUID {partuuid}. Remedy: plug it back in and restart, or remove \
                 the library. Plex will report it as an unavailable folder until one of \
                 those happens."
            ),
            Self::Vanished { name, path } => write!(
                f,
                "the drive holding {name} is attached, but {path} is not on it any more. \
                 Remedy: the folder was renamed, moved or deleted on the other computer. \
                 Remove the library and add it again from where it is now."
            ),
            Self::Mount { target, cause } => {
                // Match the remedy to the error kind. A message listing every cause is a
                // message that sends somebody to check the thing that was already fine.
                let remedy = match cause.kind() {
                    io::ErrorKind::InvalidInput => {
                        "The kernel rejected the mount options rather than the drive. \
                         That is a fault in MediaLith: the option string is built in \
                         plexosd::disks::options_for."
                    }
                    io::ErrorKind::NotFound => {
                        "The device node is not there. The drive was unplugged between \
                         the scan and now."
                    }
                    io::ErrorKind::PermissionDenied => {
                        "The kernel refused the mount itself, which read-only media does \
                         not normally produce. Check the console log for what ntfs3 or \
                         exfat wrote to the kernel ring buffer."
                    }
                    _ => {
                        "Check the drive is still attached and that its filesystem is one \
                         this kernel builds."
                    }
                };
                write!(
                    f,
                    "mounting at {} failed: {cause}. {remedy}",
                    target.display()
                )
            }
            Self::Unmountable(refusal) => write!(f, "{refusal}"),
            Self::Io(cause) => write!(f, "{cause}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(cause: io::Error) -> Self {
        Self::Io(cause)
    }
}

/// Whether a `/sys/block` entry is a real disk somebody could have plugged in.
///
/// Named rather than probed, and the exclusions are for different reasons: `loop` and
/// `dm-` are this system's own machinery, `ram` and `zram` are memory, `md` is an array
/// this image cannot assemble, and `sr` is optical — which [`crate::media`] handles and a
/// library does not live on.
fn is_disk(name: &str) -> bool {
    !name.starts_with("loop")
        && !name.starts_with("dm-")
        && !name.starts_with("ram")
        && !name.starts_with("zram")
        && !name.starts_with("md")
        && !name.starts_with("sr")
}

/// `PARTUUID` and `PARTNAME` out of a partition's `uevent`.
///
/// Deliberately more permissive than [`plexos_sys::device::parse_uevent`], which returns
/// nothing for a partition without a `PARTNAME`. That is right where it is used — PID 1
/// resolving a slot by label — and wrong here: an MBR partition never has a `PARTNAME`,
/// and an MBR drive full of films is exactly what this module exists to find.
fn identity(contents: &str) -> (Option<String>, Option<String>) {
    let mut partuuid = None;
    let mut label = None;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("PARTUUID=") {
            let value = value.trim().to_ascii_lowercase();
            if !value.is_empty() {
                partuuid = Some(value);
            }
        } else if let Some(value) = line.strip_prefix("PARTNAME=") {
            let value = value.trim();
            if !value.is_empty() {
                label = Some(value.to_owned());
            }
        }
    }
    (partuuid, label)
}

/// The labels MediaLith gives its own partitions.
///
/// Derived from the frozen layout rather than listed, so a seventh partition added to
/// ADR-0003 is covered by this without anybody remembering to come here. A rule that
/// enumerates the kinds of a thing will miss one.
fn own_partition_label(label: &str) -> Option<&'static str> {
    plexos_types::partition::LAYOUT_X86_64
        .iter()
        .map(|spec| spec.label)
        .find(|&known| known == label)
}

/// Every volume attached to this machine, with the ones that must be left alone refused.
///
/// `running` comes from [`crate::install::running_disk`], which resolves it through
/// dm-verity's `slaves` rather than by trusting a label. `None` means it could not be
/// established, and then **every** volume is refused: "I do not know" and "nothing is
/// excluded" are the same value and opposite meanings, and this project has already been
/// caught by treating them as the same.
///
/// # Errors
/// If `/sys/block` cannot be read at all. One unreadable disk is skipped rather than
/// failing the scan, because a machine with a flaky drive must still be able to find the
/// others.
pub fn volumes(env: &impl Environment, running: Option<&str>) -> io::Result<Vec<Volume>> {
    let mut found = Vec::new();

    for disk_dir in env.list_dir(Path::new(SYS_BLOCK))? {
        let Some(disk) = disk_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_disk(disk) {
            continue;
        }

        let model = env
            .read(&disk_dir.join("device/model"))
            .ok()
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "unnamed".to_owned());

        let mut partitions = Vec::new();
        if let Ok(entries) = env.list_dir(&disk_dir) {
            for entry in entries {
                let Some(part) = entry.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !part.starts_with(disk) || part == disk {
                    continue;
                }
                // A directory under a disk whose name starts with the disk's name is not
                // necessarily a partition -- `nvme0n1` has no such sibling, but a check
                // that assumes it does would offer `sda_queue` if one ever existed.
                if env.read(&entry.join("partition")).is_err() {
                    continue;
                }
                let Some(bytes) = size_of(env, &entry) else {
                    continue;
                };
                let uevent = env.read(&entry.join("uevent")).unwrap_or_default();
                let (partuuid, label) = identity(&uevent);
                partitions.push(Volume {
                    name: part.to_owned(),
                    device: format!("/dev/{part}"),
                    disk: disk.to_owned(),
                    model: model.clone(),
                    partuuid,
                    label,
                    bytes,
                    filesystem: None,
                    mounted_at: None,
                    readable_by_plex: None,
                    refusal: None,
                });
            }
        }

        // A drive with no partition table at all: a filesystem written straight to the
        // whole device, which some tools still do to USB sticks. It is offered, and it
        // cannot be remembered — there is no PARTUUID to remember it by, and `add` says
        // so rather than recording a device name that means nothing after a reboot.
        if partitions.is_empty()
            && let Some(bytes) = size_of(env, &disk_dir)
        {
            partitions.push(Volume {
                name: disk.to_owned(),
                device: format!("/dev/{disk}"),
                disk: disk.to_owned(),
                model,
                partuuid: None,
                label: None,
                bytes,
                filesystem: None,
                mounted_at: None,
                readable_by_plex: None,
                refusal: None,
            });
        }

        found.extend(partitions);
    }

    for volume in &mut found {
        volume.refusal = refusal_for(volume, running).map(|r| r.to_string());
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Size in bytes from a sysfs `size` file, which counts 512-byte sectors whatever the
/// device's logical block size is.
fn size_of(env: &impl Environment, dir: &Path) -> Option<u64> {
    let sectors: u64 = env.read(&dir.join("size")).ok()?.trim().parse().ok()?;
    (sectors != 0).then(|| sectors * 512)
}

/// Why this volume is not offered, if it is not.
fn refusal_for(volume: &Volume, running: Option<&str>) -> Option<Refusal> {
    let Some(running) = running else {
        return Some(Refusal::SourceUnknown);
    };
    if volume.disk == running {
        return Some(Refusal::RunningDisk(running.to_owned()));
    }
    let label = volume.label.as_deref()?;
    own_partition_label(label).map(|known| Refusal::OwnPartition(known.to_owned()))
}

/// Mounts a volume read-only, trying each filesystem in turn.
///
/// Returns the one that worked, so a failure can say what was tried. Read-only always;
/// see the module header for why that is also what makes a hibernated Windows partition
/// readable.
///
/// # Errors
/// If none of [`FILESYSTEMS`] mounts it. [`Refusal::Unmountable`] names them all and the
/// two causes worth naming — `BitLocker` and a filesystem this kernel does not build —
/// because "mount failed" sends nobody anywhere.
pub fn mount_probe(device: &str, target: &Path) -> Result<String, Refusal> {
    if let Err(cause) = std::fs::create_dir_all(target) {
        return Err(Refusal::Unmountable {
            device: device.to_owned(),
            cause: format!("{} could not be created: {cause}", target.display()),
        });
    }
    let mut last = "nothing was tried".to_owned();
    for fstype in FILESYSTEMS {
        match plexos_sys::mount::mount(
            device,
            &target.to_string_lossy(),
            fstype,
            &options_for(fstype),
        ) {
            Ok(()) => return Ok(fstype.to_owned()),
            Err(error) => last = format!("{fstype}: {error}"),
        }
    }
    let _ = std::fs::remove_dir(target);
    Err(Refusal::Unmountable {
        device: device.to_owned(),
        cause: last,
    })
}

/// Whether uid 900 can list a directory.
///
/// Asked rather than assumed, and asked of the *mounted* directory rather than of the
/// filesystem's name. The three filesystems with no ownership are made readable by
/// [`options_for`]; ext4 and XFS carry ownership this cannot change, so a library written
/// by another Linux machine may be closed to Plex. That has to be reported at the moment
/// somebody chooses the folder, because the alternative is a library that scans as empty
/// with nothing anywhere saying why.
///
/// Checked from the mode bits rather than by trying it: this process is root, and root
/// can read anything, so *attempting* the read answers about the wrong process. Same
/// mistake as a GPU report that probes as root.
#[must_use]
pub fn readable_by(path: &Path, uid: u32, gid: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let mode = meta.mode();
    // Directory traversal needs read and execute together: read to list it, execute to
    // walk into it. Either alone is a directory Plex cannot use.
    let wanted = 0o5;
    if meta.uid() == uid {
        return (mode >> 6) & wanted == wanted;
    }
    if meta.gid() == gid {
        return (mode >> 3) & wanted == wanted;
    }
    mode & wanted == wanted
}

/// What the last scan found out, kept because nothing else can reconstruct it.
///
/// A scan is the only thing that learns *why* a drive will not mount, and that knowledge
/// exists nowhere on the machine afterwards: the volume is still attached, still
/// enumerable, and simply not mounted. [`report`] re-derives everything else from the
/// state of the machine, and the state of a machine cannot express "this was tried and it
/// failed" — it looks exactly like "this has not been tried".
///
/// That was shipped, and it cost the first real scan on hardware. `mount_probe` produced a
/// refusal naming what each of six filesystems said, the console threw the response away
/// and asked again with a `GET`, and the page then reported the drive as "not scanned yet"
/// — a diagnosis that existed, was correct, and reached nobody. Same shape as a rollback
/// destroying its own explanation, at a much smaller scale.
///
/// A `Mutex` rather than a field on anything: `plexosd` serves each connection on its own
/// thread, and the browser that reads the outcome is usually not the request that produced
/// it. Cleared by nothing — a later scan replaces it, and a reboot loses it along with the
/// mounts it describes, which is correct.
static LAST_SCAN: std::sync::Mutex<Option<ScanOutcome>> = std::sync::Mutex::new(None);

/// What one scan learnt, by volume name.
#[derive(Debug, Clone, Default)]
struct ScanOutcome {
    /// Volumes that would not mount, and what was said about each.
    refused: std::collections::BTreeMap<String, String>,
}

/// Mounts every offered volume so its contents can be looked at.
///
/// One volume failing does not stop the others: a drive with a filesystem this kernel
/// cannot read should cost itself and nothing else. A volume already mounted is left
/// alone, so scanning twice is not a way to lose the folder somebody was looking at.
///
/// What it learnt is recorded in `LAST_SCAN` as well as returned, so the answer survives
/// the response that carried it.
///
/// # Errors
/// If the volumes cannot be enumerated at all.
pub fn scan(
    env: &impl Environment,
    running: Option<&str>,
    log: &mut dyn FnMut(&str),
) -> io::Result<Vec<Volume>> {
    let mut found = volumes(env, running)?;
    std::fs::create_dir_all(SCAN_ROOT)?;
    let mut outcome = ScanOutcome::default();

    for volume in &mut found {
        if volume.refusal.is_some() {
            continue;
        }
        let target = volume.scan_point();
        if crate::shares::is_mounted(&target) {
            volume.mounted_at = Some(target.display().to_string());
            volume.filesystem = mounted_filesystem(&target);
            volume.readable_by_plex = Some(readable_by(&target, PLEX_UID, PLEX_UID));
            continue;
        }
        match mount_probe(&volume.device, &target) {
            Ok(fstype) => {
                log(&format!(
                    "{} mounted as {fstype} at {}",
                    volume.device,
                    target.display()
                ));
                volume.readable_by_plex = Some(readable_by(&target, PLEX_UID, PLEX_UID));
                volume.filesystem = Some(fstype);
                volume.mounted_at = Some(target.display().to_string());
            }
            Err(refusal) => {
                let said = refusal.to_string();
                log(&format!("{}: {said}", volume.device));
                outcome.refused.insert(volume.name.clone(), said.clone());
                volume.refusal = Some(said);
            }
        }
    }

    if let Ok(mut last) = LAST_SCAN.lock() {
        *last = Some(outcome);
    }
    Ok(found)
}

/// Puts the last scan's refusal back on a volume that is not mounted and not otherwise
/// refused.
///
/// A function rather than three lines inside [`report`], so the rule can be tested without
/// a disk. The rule is narrow on purpose: a volume MediaLith refuses for what it *is* —
/// its own partition, the running disk — keeps that reason, because it is the truer one
/// and it would still be true if the scan had never run.
fn carry_over(volume: &mut Volume, last: Option<&ScanOutcome>) {
    if volume.mounted_at.is_some() || volume.refusal.is_some() {
        return;
    }
    if let Some(said) = last.and_then(|l| l.refused.get(&volume.name)) {
        volume.refusal = Some(said.clone());
    }
}

/// Forgets what the last scan learnt.
///
/// For [`release_unused`], which unmounts what a scan mounted: leaving the outcome behind
/// would report drives as refused when the honest answer is that nothing has been tried
/// since.
fn forget_last_scan() {
    if let Ok(mut last) = LAST_SCAN.lock() {
        *last = None;
    }
}

/// What `/proc/mounts` says is mounted at a path.
#[must_use]
pub fn mounted_filesystem(target: &Path) -> Option<String> {
    let wanted = target.to_string_lossy();
    std::fs::read_to_string("/proc/mounts")
        .ok()?
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(wanted.as_ref()))
        .and_then(|line| line.split_whitespace().nth(2))
        .map(ToOwned::to_owned)
}

/// Whether a file name is something Plex would play.
#[must_use]
pub fn looks_like_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTENSIONS.iter().any(|k| e.eq_ignore_ascii_case(k)))
}

/// Lists the directories inside one directory, so somebody can choose one.
///
/// Reads the real filesystem rather than going through [`Environment`]. That abstraction
/// exists so sysfs can be faked; this walks a filesystem that only exists once something
/// has been mounted, and a fixture of it would describe a machine where the mount had
/// already succeeded — the shape of test this project has been burned by.
///
/// Symlinks are not followed. A link on somebody's Windows disk can point anywhere, and a
/// followed one would be offered and then correctly refused by [`vet`], which reads as
/// the console being broken.
///
/// # Errors
/// If the directory cannot be read.
pub fn browse(dir: &Path) -> io::Result<Listing> {
    let mut entries = Vec::new();
    let mut media_here = 0;
    let mut truncated = false;

    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let Ok(kind) = item.file_type() else { continue };
        let path = item.path();
        if kind.is_file() {
            if looks_like_media(&path) {
                media_here += 1;
            }
            continue;
        }
        if !kind.is_dir() {
            continue;
        }
        if entries.len() >= BROWSE_LIMIT {
            truncated = true;
            continue;
        }
        let (media, folders) = shallow_count(&path);
        entries.push(Entry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            media_here: media,
            folders,
        });
    }

    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    Ok(Listing {
        path: dir.to_string_lossy().into_owned(),
        parent: parent_within_scan(dir),
        entries,
        media_here,
        truncated,
    })
}

/// The directory above, or `None` at the top of a volume.
///
/// The stop is at the volume's own mount point rather than at [`SCAN_ROOT`], so walking up
/// cannot cross from one drive into the list of drives — which would present the scan
/// directory as though it were a folder on somebody's disk.
fn parent_within_scan(dir: &Path) -> Option<String> {
    let parent = dir.parent()?;
    if parent == Path::new(SCAN_ROOT) || !parent.starts_with(SCAN_ROOT) {
        return None;
    }
    Some(parent.to_string_lossy().into_owned())
}

/// Media files and subdirectories directly inside a directory, without descending.
///
/// One level, because the count exists to tell `Movies` from `System Volume Information`
/// and walking a Windows disk to do it would make a folder listing take minutes on the
/// medium where it is slowest.
fn shallow_count(dir: &Path) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut media = 0;
    let mut folders = 0;
    for item in entries.flatten().take(4096) {
        match item.file_type() {
            Ok(kind) if kind.is_dir() => folders += 1,
            Ok(kind) if kind.is_file() && looks_like_media(&item.path()) => media += 1,
            _ => {}
        }
    }
    (media, folders)
}

/// Resolves a path the console was given and refuses anything not on a scanned volume.
///
/// The check is on the **canonical** path, so `..` cannot walk out of the scan root.
/// Comparing the string as given would accept
/// `/run/plexos/disks/<uuid>/../../../var/lib/plexos/device-token`, and this route would
/// then hand any directory on the appliance to whoever named one. The credential is
/// required either way; that is not a reason to leave the hole.
///
/// # Errors
/// [`Error::NotOnAVolume`] for a path outside [`SCAN_ROOT`] or one that no longer exists.
pub fn vet(chosen: &str) -> Result<PathBuf, Error> {
    let canonical =
        std::fs::canonicalize(chosen).map_err(|_| Error::NotOnAVolume(chosen.to_owned()))?;
    if canonical.starts_with(SCAN_ROOT) && canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(Error::NotOnAVolume(chosen.to_owned()))
    }
}

/// Splits a vetted path into the volume it is on and the part inside it.
///
/// The first component under [`SCAN_ROOT`] is the volume key, which [`Volume::scan_point`]
/// made the `PARTUUID`. Returns `None` for the scan root itself, which is not on a volume.
#[must_use]
pub fn split_scan_path(canonical: &Path) -> Option<(String, String)> {
    let rest = canonical.strip_prefix(SCAN_ROOT).ok()?;
    let mut parts = rest.components();
    let key = parts.next()?.as_os_str().to_str()?.to_owned();
    let subpath = parts.as_path().to_string_lossy().into_owned();
    Some((key, subpath))
}

/// The remembered libraries, or none.
///
/// A missing or unreadable file means no libraries, not an error: that is the state of
/// every appliance until somebody adds one, and refusing to boot over a truncated JSON
/// file would be a poor trade.
#[must_use]
pub fn load() -> Vec<Library> {
    std::fs::read_to_string(CONFIG)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes the list back.
///
/// # Errors
/// If the state directory cannot be written.
pub fn save(libraries: &[Library]) -> Result<(), Error> {
    if let Some(parent) = Path::new(CONFIG).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(libraries).map_err(io::Error::other)?;
    std::fs::write(CONFIG, text)?;
    Ok(())
}

/// Bind-mounts a directory to where Plex is granted to look.
///
/// Only `bind` is passed, and that is deliberate rather than an omission: `mount(2)`
/// discards every other flag for a bind (`fs/namespace.c:4025`) and the new mount copies
/// the source's flags verbatim (`fs/namespace.c:1256`). The result is read-only because
/// the volume under it is. An option string here that listed `ro,noexec` would read as
/// though it were enforcing them.
///
/// # Errors
/// If the mount point cannot be made or the bind fails.
pub fn bind(source: &Path, target: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(target)?;
    plexos_sys::mount::mount(
        &source.to_string_lossy(),
        &target.to_string_lossy(),
        "none",
        "bind",
    )
    .map_err(|cause| Error::Mount {
        target: target.to_owned(),
        cause,
    })
}

/// The device node for a partition with this `PARTUUID`, if it is attached.
///
/// # Errors
/// If `/sys/block` cannot be read.
pub fn device_with_partuuid(env: &impl Environment, partuuid: &str) -> io::Result<Option<String>> {
    let wanted = partuuid.to_ascii_lowercase();
    Ok(volumes(env, None)?
        .into_iter()
        .find(|v| v.partuuid.as_deref() == Some(wanted.as_str()))
        .map(|v| v.device))
}

/// Mounts one remembered library: the volume under [`SCAN_ROOT`], then the bind.
///
/// # Errors
/// [`Error::Absent`] if the drive is not attached, [`Error::Vanished`] if it is and the
/// folder is not, and the mount errors otherwise. Each names what to do about it.
pub fn mount_one(
    env: &impl Environment,
    library: &Library,
    log: &mut dyn FnMut(&str),
) -> Result<(), Error> {
    let target = library.mount_point();
    if crate::shares::is_mounted(&target) {
        return Ok(());
    }

    let scan_point = Path::new(SCAN_ROOT).join(&library.partuuid);
    if !crate::shares::is_mounted(&scan_point) {
        let Some(device) = device_with_partuuid(env, &library.partuuid)? else {
            return Err(Error::Absent {
                name: library.name.clone(),
                partuuid: library.partuuid.clone(),
            });
        };
        let fstype = mount_probe(&device, &scan_point).map_err(Error::Unmountable)?;
        log(&format!(
            "{} mounted as {fstype} for library {}",
            device, library.name
        ));
    }

    let source = if library.subpath.is_empty() {
        scan_point
    } else {
        scan_point.join(&library.subpath)
    };
    if !source.is_dir() {
        return Err(Error::Vanished {
            name: library.name.clone(),
            path: source.display().to_string(),
        });
    }

    bind(&source, &target)?;
    if !readable_by(&target, PLEX_UID, PLEX_UID) {
        // Not an error: the library is mounted and a person can see it on the page. It
        // is a warning because Plex will scan it as empty, and an empty library with
        // nothing anywhere saying why is the outcome this whole module is arranged to
        // avoid.
        log(&format!(
            "{} is mounted and is not readable by uid {PLEX_UID}, which is the account \
             Plex runs as, so it will scan as empty. Remedy: this is an ext4 or XFS \
             filesystem carrying ownership from the machine that wrote it. Make it \
             world-readable there, or copy the library to a drive formatted exFAT or NTFS.",
            target.display()
        ));
    }
    Ok(())
}

/// Turns a vetted scan path into the record that would be kept for it.
///
/// Separate from [`add`] and pure, because everything that can be got wrong here is
/// decided before anything is mounted: which volume the path is on, whether that volume
/// has an identifier worth recording, and whether the name is a name. A function that
/// only exists inside `add` is one that can only be tested by mounting a drive, and a
/// fixture of a mounted drive is a fixture of the state after the thing being tested has
/// already worked.
///
/// # Errors
/// [`Error::NotOnAVolume`] if the path is not under a volume this scan saw,
/// [`Error::Unrememberable`] if that volume has no `PARTUUID`, [`Error::BadName`] if the
/// name could not be a directory.
pub fn record_for(name: &str, canonical: &Path, volumes_seen: &[Volume]) -> Result<Library, Error> {
    let Some((key, subpath)) = split_scan_path(canonical) else {
        return Err(Error::NotOnAVolume(canonical.display().to_string()));
    };
    let Some(volume) = volumes_seen
        .iter()
        .find(|v| v.scan_point() == Path::new(SCAN_ROOT).join(&key))
    else {
        return Err(Error::NotOnAVolume(canonical.display().to_string()));
    };

    // The scan key is the PARTUUID when there is one and the device name when there is
    // not -- and a device name is exactly what must not be recorded.
    let Some(partuuid) = volume.partuuid.clone() else {
        return Err(Error::Unrememberable {
            volume: volume.device.clone(),
        });
    };

    let library = Library {
        name: name.to_owned(),
        partuuid,
        subpath,
        filesystem: volume.filesystem.clone(),
    };
    if !library.has_safe_name() {
        return Err(Error::BadName(name.to_owned()));
    }
    Ok(library)
}

/// Adds a library from a chosen path, mounts it, and records it.
///
/// Recorded **after** it has mounted, so a folder that cannot be brought up does not
/// become an entry that fails on every boot afterwards.
///
/// # Errors
/// See [`Error`].
pub fn add(name: &str, chosen: &str, volumes_seen: &[Volume]) -> Result<Library, Error> {
    let canonical = vet(chosen)?;
    let library = record_for(name, &canonical, volumes_seen)?;

    let mut all = load();
    if all.iter().any(|existing| existing.name == library.name) {
        return Err(Error::Duplicate(name.to_owned()));
    }

    bind(&canonical, &library.mount_point())?;
    all.push(library.clone());
    save(&all)?;
    Ok(library)
}

/// Unmounts a library and forgets it.
///
/// # Errors
/// If the record cannot be written. A failed unmount is reported and the entry is
/// removed anyway: the alternative is a library nobody can get rid of because something
/// has a file open on it, and the mount goes at the next restart regardless.
pub fn remove(name: &str, log: &mut dyn FnMut(&str)) -> Result<bool, Error> {
    let mut all = load();
    let Some(index) = all.iter().position(|library| library.name == name) else {
        return Ok(false);
    };
    let library = all.remove(index);
    let target = library.mount_point();
    if crate::shares::is_mounted(&target)
        && let Err(error) = plexos_sys::mount::unmount(&target.to_string_lossy())
    {
        log(&format!(
            "{} could not be unmounted: {error}. It is no longer configured and will not \
             come back after a restart.",
            target.display()
        ));
    }
    let _ = std::fs::remove_dir(&target);
    save(&all)?;
    Ok(true)
}

/// Mounts every remembered library, reporting each.
///
/// Called before Plex starts, for the reason in the module header. One library failing
/// does not stop the others: a drive somebody unplugged should cost its own folder and
/// nothing else.
pub fn mount_all(env: &impl Environment, log: &mut dyn FnMut(&str)) {
    let libraries = load();
    if libraries.is_empty() {
        return;
    }
    if let Err(error) = std::fs::create_dir_all(ROOT) {
        log(&format!("could not create {ROOT}: {error}"));
        return;
    }
    for library in &libraries {
        if let Err(error) = mount_one(env, library, log) {
            log(&format!("{}: {error}", library.name));
        }
    }
}

/// One library and whether it is there right now, for reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct State {
    /// The record.
    #[serde(flatten)]
    pub library: Library,
    /// Where it appears.
    pub mount_point: String,
    /// Whether it is mounted right now.
    pub mounted: bool,
    /// Whether the drive it is on is attached at all.
    ///
    /// Separate from `mounted`, because they take opposite remedies: a drive that is not
    /// there wants plugging in, and one that is there and not mounted is a fault to read
    /// the log about.
    pub present: bool,
}

/// Every remembered library, with its current state.
///
/// # Errors
/// If `/sys/block` cannot be read.
pub fn states(env: &impl Environment) -> io::Result<Vec<State>> {
    let attached = volumes(env, None)?;
    Ok(load()
        .into_iter()
        .map(|library| State {
            mount_point: library.mount_point().display().to_string(),
            mounted: crate::shares::is_mounted(&library.mount_point()),
            present: attached
                .iter()
                .any(|v| v.partuuid.as_deref() == Some(library.partuuid.as_str())),
            library,
        })
        .collect())
}

/// Unmounts every scan mount that is not holding up a library.
///
/// So that a drive somebody looked through and did not choose can be unplugged without
/// waiting for a reboot. A scan point backing a library is left alone even though a bind
/// keeps its own reference to the mount tree: relying on that would be a correct piece of
/// reasoning about the kernel standing between somebody and their films, and there is no
/// reason to need it.
pub fn release_unused(log: &mut dyn FnMut(&str)) {
    forget_last_scan();
    let kept: Vec<String> = load().into_iter().map(|l| l.partuuid).collect();
    let Ok(entries) = std::fs::read_dir(SCAN_ROOT) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(key) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if kept.iter().any(|k| k == key) {
            continue;
        }
        if crate::shares::is_mounted(&path)
            && let Err(error) = plexos_sys::mount::unmount(&path.to_string_lossy())
        {
            log(&format!(
                "{} is still in use and was not unmounted: {error}. Remedy: nothing is \
                 wrong with the drive; something on this appliance still has a file open \
                 on it. It goes at the next restart.",
                path.display()
            ));
            continue;
        }
        let _ = std::fs::remove_dir(&path);
    }
}

/// What `GET /api/disks` answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The libraries this appliance remembers, and whether each is there.
    pub libraries: Vec<State>,
    /// Every volume attached right now, whether or not anything has mounted it.
    pub volumes: Vec<Volume>,
    /// Where volumes appear while they are being looked through.
    pub scan_root: String,
    /// Whether anything has been mounted for browsing this boot.
    ///
    /// The page needs to tell "nobody has scanned yet" from "scanned and found nothing",
    /// which are the same empty list and take opposite words.
    pub scanned: bool,
}

/// The inventory, without mounting anything.
///
/// A `GET`, so it needs no credential — a broken machine has to stay diagnosable. What it
/// discloses is what `/api/install` already does: device names, models, sizes and GPT
/// partition names. Looking *inside* a drive is [`handle`]'s `browse`, which is a `POST`
/// and therefore behind the administrator credential, because the folder names on
/// somebody's Windows disk are not something every reader on the LAN should have.
///
/// # Errors
/// If `/sys/block` cannot be read.
pub fn report(env: &impl Environment) -> io::Result<Report> {
    let running = crate::install::running_disk(env);
    let mut volumes = volumes(env, running.as_deref())?;

    // Everything above is derived from the machine as it is now. That is right for what is
    // *there* and cannot express what was *tried*, so the last scan's answer is merged in
    // rather than recomputed -- see LAST_SCAN. Without this a drive that refused every
    // filesystem comes back looking untouched, and `scanned` is inferred from "is anything
    // mounted", which is false for a scan where nothing would mount: the page then invites
    // somebody to run the scan that has just failed, and says nothing about the failure.
    let last = LAST_SCAN.lock().ok().and_then(|l| l.clone());
    let scanned = last.is_some();

    for volume in &mut volumes {
        let point = volume.scan_point();
        if crate::shares::is_mounted(&point) {
            volume.mounted_at = Some(point.display().to_string());
            volume.filesystem = mounted_filesystem(&point);
            volume.readable_by_plex = Some(readable_by(&point, PLEX_UID, PLEX_UID));
            continue;
        }
        carry_over(volume, last.as_ref());
    }

    Ok(Report {
        libraries: states(env)?,
        volumes,
        scan_root: SCAN_ROOT.to_owned(),
        scanned,
    })
}

/// `POST /api/disks`.
///
/// Every action here mounts, unmounts or looks inside somebody's disk, so there is no
/// safe default and none is guessed. The method-based gate has already required the
/// administrator credential by the time this is reached.
pub fn handle(body: &[u8], log: &mut dyn FnMut(&str)) -> crate::http::Response {
    use crate::http::Response;

    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Response::text(400, "the request body is not JSON\n");
    };
    let Some(action) = string_in(&request, "action") else {
        return Response::text(
            400,
            "say which action: \"scan\", \"browse\", \"add\", \"remove\" or \"release\". \
             Nothing is assumed, because these mount and unmount filesystems and there is \
             no safe guess among them.\n",
        );
    };

    match action.as_str() {
        "scan" => match scan(
            &System,
            crate::install::running_disk(&System).as_deref(),
            log,
        ) {
            Ok(volumes) => json_or_500(&volumes),
            Err(error) => {
                Response::text(500, format!("the drives could not be scanned: {error}\n"))
            }
        },
        "browse" => do_browse(&request),
        "add" => do_add(&request, log),
        "remove" => do_remove(&request, log),
        "release" => {
            release_unused(log);
            match report(&System) {
                Ok(report) => json_or_500(&report),
                Err(error) => Response::text(500, format!("{error}\n")),
            }
        }
        other => Response::text(
            400,
            format!("{other:?} is not an action; use scan, browse, add, remove or release\n"),
        ),
    }
}

/// One string field from a request body.
fn string_in(request: &serde_json::Value, name: &str) -> Option<String> {
    request
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

/// Serialises, or says the serialisation failed rather than sending half a document.
fn json_or_500<T: Serialize>(value: &T) -> crate::http::Response {
    use crate::http::Response;
    match serde_json::to_string(value) {
        Ok(text) => Response::json(text),
        Err(error) => Response::text(500, format!("could not serialise the answer: {error}\n")),
    }
}

/// Lists one directory on a scanned drive.
fn do_browse(request: &serde_json::Value) -> crate::http::Response {
    use crate::http::Response;

    // No path means the top of the scan root, which is the list of drives rather than a
    // directory on one. The page asks for a volume's own mount point instead, so this is
    // a refusal rather than a default: guessing here would answer a different question.
    let Some(path) = string_in(request, "path") else {
        return Response::text(400, "browse needs a path, from a scan\n");
    };
    let vetted = match vet(&path) {
        Ok(vetted) => vetted,
        Err(error) => return Response::text(400, format!("{error}\n")),
    };
    match browse(&vetted) {
        Ok(listing) => json_or_500(&listing),
        Err(error) => Response::text(
            500,
            format!(
                "{} could not be read: {error}. Remedy: the drive may have been unplugged \
                 since it was scanned. Scan again.\n",
                vetted.display()
            ),
        ),
    }
}

/// Adds a chosen folder as a library and mounts it where Plex will find it.
fn do_add(request: &serde_json::Value, log: &mut dyn FnMut(&str)) -> crate::http::Response {
    use crate::http::Response;

    let (Some(name), Some(path)) = (string_in(request, "name"), string_in(request, "path")) else {
        return Response::text(400, "add needs a name and a path\n");
    };
    let running = crate::install::running_disk(&System);
    let seen = match volumes(&System, running.as_deref()) {
        Ok(seen) => seen,
        Err(error) => return Response::text(500, format!("{error}\n")),
    };
    match add(&name, &path, &seen) {
        Ok(library) => {
            log(&format!(
                "library {} added from {} on PARTUUID {}",
                library.name, library.subpath, library.partuuid
            ));
            let readable = readable_by(&library.mount_point(), PLEX_UID, PLEX_UID);
            json_or_500(&serde_json::json!({
                "added": library,
                // Reported rather than assumed. An ext4 or XFS drive written by another
                // Linux machine carries ownership this cannot change, and a library that
                // silently scans as empty is the outcome worth spending a field on.
                "readable_by_plex": readable,
                // Plex is confined at the moment it starts, from the paths that exist
                // then. Whether this one is reachable without a restart is the question
                // this project has been caught guessing at, so the page offers a restart
                // rather than claiming it is not needed.
                "restart_plex": true,
            }))
        }
        Err(error) => Response::text(400, format!("{error}\n")),
    }
}

/// Forgets a library and unmounts it.
fn do_remove(request: &serde_json::Value, log: &mut dyn FnMut(&str)) -> crate::http::Response {
    use crate::http::Response;

    let Some(name) = string_in(request, "name") else {
        return Response::text(400, "remove needs a name\n");
    };
    match remove(&name, log) {
        Ok(true) => Response::json("{\"removed\":true}"),
        Ok(false) => Response::text(404, format!("there is no library called {name:?}\n")),
        Err(error) => Response::text(500, format!("{error}\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_gpu::env::Fixture;

    /// A GPT partition as the kernel describes one, taken from the build host's own
    /// Windows disk rather than composed: `PARTN` sits directly above `PARTNAME`, which
    /// is the adjacency a prefix match has to get right.
    fn gpt_uevent(name: &str, n: u32, label: &str, uuid: &str) -> String {
        format!(
            "MAJOR=259\nMINOR={n}\nDEVNAME={name}\nDEVTYPE=partition\nDISKSEQ=9\n\
             PARTN={n}\nPARTNAME={label}\nPARTUUID={uuid}\n"
        )
    }

    /// An MBR partition. The kernel emits no PARTNAME at all for one, and synthesises the
    /// PARTUUID from the disk signature and the partition number
    /// (`block/partitions/msdos.c:113`).
    fn mbr_uevent(name: &str, n: u32, uuid: &str) -> String {
        format!(
            "MAJOR=8\nMINOR={n}\nDEVNAME={name}\nDEVTYPE=partition\nDISKSEQ=3\n\
             PARTN={n}\nPARTUUID={uuid}\n"
        )
    }

    /// A laptop with Windows on an internal `NVMe` and MediaLith running from a USB stick.
    /// The arrangement this module was written for.
    fn windows_laptop() -> Fixture {
        Fixture::new()
            .file("/sys/block/nvme0n1/size", "1000215216\n")
            .file("/sys/block/nvme0n1/device/model", "KINGSTON SA2000M8\n")
            .file("/sys/block/nvme0n1/nvme0n1p1/size", "204800\n")
            .file("/sys/block/nvme0n1/nvme0n1p1/partition", "1\n")
            .file(
                "/sys/block/nvme0n1/nvme0n1p1/uevent",
                gpt_uevent(
                    "nvme0n1p1",
                    1,
                    "EFI system partition",
                    "0AE58AFC-3657-403E-BD6B-581940113DB4",
                ),
            )
            .file("/sys/block/nvme0n1/nvme0n1p2/size", "999000000\n")
            .file("/sys/block/nvme0n1/nvme0n1p2/partition", "2\n")
            .file(
                "/sys/block/nvme0n1/nvme0n1p2/uevent",
                gpt_uevent(
                    "nvme0n1p2",
                    2,
                    "Basic data partition",
                    "b1cf4f2e-9a71-4d0b-8f2c-5d3e7a10c944",
                ),
            )
            .file("/sys/block/sda/size", "60000000\n")
            .file("/sys/block/sda/device/model", "Ultra Fit\n")
            .file("/sys/block/sda/sda1/size", "1048576\n")
            .file("/sys/block/sda/sda1/partition", "1\n")
            .file(
                "/sys/block/sda/sda1/uevent",
                gpt_uevent("sda1", 1, "esp", "6d1e5f00-0000-4000-8000-000000000001"),
            )
    }

    #[test]
    fn a_windows_data_partition_is_offered_and_the_running_disk_is_not() {
        let found = volumes(&windows_laptop(), Some("sda")).expect("enumerated");

        let data = found
            .iter()
            .find(|v| v.name == "nvme0n1p2")
            .expect("the Windows data partition");
        assert!(data.refusal.is_none(), "{:?}", data.refusal);
        assert_eq!(
            data.partuuid.as_deref(),
            Some("b1cf4f2e-9a71-4d0b-8f2c-5d3e7a10c944")
        );
        assert_eq!(data.label.as_deref(), Some("Basic data partition"));

        let stick = found.iter().find(|v| v.name == "sda1").expect("the stick");
        assert!(
            stick.refusal.as_deref().is_some_and(|r| r.contains("sda")),
            "the disk MediaLith is running from must be refused: {:?}",
            stick.refusal
        );
    }

    #[test]
    fn a_partuuid_is_lower_cased_so_two_spellings_of_one_partition_are_one_partition() {
        // The kernel prints GPT GUIDs lower case in `uevent` and firmware reports them
        // upper case. A library recorded from one and looked up by the other would be a
        // drive that is attached and reported absent.
        let found = volumes(&windows_laptop(), Some("sda")).expect("enumerated");
        let esp = found.iter().find(|v| v.name == "nvme0n1p1").expect("esp");
        assert_eq!(
            esp.partuuid.as_deref(),
            Some("0ae58afc-3657-403e-bd6b-581940113db4")
        );
    }

    #[test]
    fn an_mbr_partition_has_no_label_and_is_still_offered() {
        // parse_uevent in plexos-sys returns None for this, correctly, and using it here
        // would make every MBR drive invisible -- which is most USB drives Windows has
        // ever partitioned.
        let env = Fixture::new()
            .file("/sys/block/sdb/size", "3907029168\n")
            .file("/sys/block/sdb/device/model", "Elements 25A3\n")
            .file("/sys/block/sdb/sdb1/size", "3907027120\n")
            .file("/sys/block/sdb/sdb1/partition", "1\n")
            .file(
                "/sys/block/sdb/sdb1/uevent",
                mbr_uevent("sdb1", 1, "6f20736b-01"),
            );

        let found = volumes(&env, Some("sda")).expect("enumerated");
        let drive = found.iter().find(|v| v.name == "sdb1").expect("the drive");
        assert!(drive.label.is_none(), "MBR partitions carry no PARTNAME");
        assert_eq!(drive.partuuid.as_deref(), Some("6f20736b-01"));
        assert!(drive.refusal.is_none(), "{:?}", drive.refusal);
    }

    #[test]
    fn medialiths_own_partitions_on_another_disk_are_refused_by_the_frozen_layout() {
        // Derived from LAYOUT_X86_64 rather than from a list here, so the check covers a
        // label added to ADR-0003 without anybody remembering this file.
        for spec in plexos_types::partition::LAYOUT_X86_64 {
            let env = Fixture::new()
                .file("/sys/block/sdc/size", "60000000\n")
                .file("/sys/block/sdc/sdc1/size", "1048576\n")
                .file("/sys/block/sdc/sdc1/partition", "1\n")
                .file(
                    "/sys/block/sdc/sdc1/uevent",
                    gpt_uevent(
                        "sdc1",
                        1,
                        spec.label,
                        "aaaaaaaa-0000-4000-8000-000000000001",
                    ),
                );
            let found = volumes(&env, Some("sda")).expect("enumerated");
            let own = found.iter().find(|v| v.name == "sdc1").expect("partition");
            assert!(
                own.refusal
                    .as_deref()
                    .is_some_and(|r| r.contains(spec.label)),
                "{} must be refused as one of MediaLith's own: {:?}",
                spec.label,
                own.refusal
            );
        }
    }

    #[test]
    fn not_knowing_the_running_disk_refuses_everything_rather_than_nothing() {
        // "I do not know" and "nothing is excluded" are the same value and opposite
        // meanings. The installer learned this the expensive way.
        let found = volumes(&windows_laptop(), None).expect("enumerated");
        assert!(!found.is_empty());
        assert!(
            found.iter().all(|v| v
                .refusal
                .as_deref()
                .is_some_and(|r| r.contains("cannot identify the disk it booted from"))),
            "every volume must be refused when the running disk is unknown"
        );
    }

    #[test]
    fn a_whole_disk_filesystem_is_offered_and_cannot_be_remembered() {
        let env = Fixture::new()
            .file("/sys/block/sdb/size", "15728640\n")
            .file("/sys/block/sdb/device/model", "Cruzer Blade\n");
        let found = volumes(&env, Some("sda")).expect("enumerated");
        let whole = found.iter().find(|v| v.name == "sdb").expect("the stick");
        assert!(whole.partuuid.is_none());
        assert!(whole.refusal.is_none(), "it can still be browsed");

        // And choosing a folder on it says why, rather than recording "sdb" -- which is
        // whatever the kernel called it during one boot and means nothing after the next.
        let error = record_for("films", Path::new("/run/plexos/disks/sdb/Films"), &found)
            .expect_err("a drive with no partition table cannot be remembered");
        assert!(matches!(error, Error::Unrememberable { .. }), "{error}");
        let said = error.to_string();
        assert!(said.contains("no partition table"), "{said}");
        assert!(said.contains("Remedy:"), "{said}");
    }

    #[test]
    fn a_folder_on_a_partitioned_drive_is_recorded_by_partuuid_and_the_path_inside_it() {
        let found = volumes(&windows_laptop(), Some("sda")).expect("enumerated");
        let library = record_for(
            "films",
            Path::new("/run/plexos/disks/b1cf4f2e-9a71-4d0b-8f2c-5d3e7a10c944/Users/seb/Videos"),
            &found,
        )
        .expect("recorded");

        assert_eq!(library.partuuid, "b1cf4f2e-9a71-4d0b-8f2c-5d3e7a10c944");
        assert_eq!(library.subpath, "Users/seb/Videos");
        assert_eq!(library.mount_point(), Path::new("/var/media/films"));
        // Nothing about `sdb1` or `nvme0n1p2` is kept. That is the whole point: the
        // record has to survive the kernel renumbering the drives.
        let json = serde_json::to_string(&library).expect("serialised");
        assert!(!json.contains("nvme0n1p2"), "{json}");
    }

    #[test]
    fn a_folder_on_a_volume_this_scan_did_not_see_is_refused() {
        // A page holding a path from a scan taken before somebody unplugged the drive.
        // Recording it would produce a library that has never been mountable.
        let found = volumes(&windows_laptop(), Some("sda")).expect("enumerated");
        let error = record_for(
            "films",
            Path::new("/run/plexos/disks/ffffffff-0000-4000-8000-000000000000/Films"),
            &found,
        )
        .expect_err("no such volume in this scan");
        assert!(matches!(error, Error::NotOnAVolume(_)), "{error}");
    }

    #[test]
    fn a_name_that_could_not_be_a_directory_is_refused_before_anything_is_mounted() {
        let found = volumes(&windows_laptop(), Some("sda")).expect("enumerated");
        let error = record_for(
            "../../etc",
            Path::new("/run/plexos/disks/b1cf4f2e-9a71-4d0b-8f2c-5d3e7a10c944/Films"),
            &found,
        )
        .expect_err("a name is a directory name");
        assert!(matches!(error, Error::BadName(_)), "{error}");
    }

    #[test]
    fn loop_and_device_mapper_devices_are_not_drives_somebody_plugged_in() {
        let env = Fixture::new()
            .file("/sys/block/loop0/size", "204800\n")
            .file("/sys/block/dm-0/size", "2097152\n")
            .file("/sys/block/sr0/size", "1234\n")
            .file("/sys/block/zram0/size", "4096\n");
        let found = volumes(&env, Some("sda")).expect("enumerated");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn every_filesystem_with_no_unix_ownership_is_mounted_with_ownership_supplied() {
        // The render-node defect in a new place: ntfs3, exfat and vfat take uid and
        // permissions from the mounting process (fs/ntfs3/super.c:1804), and plexosd is
        // root. Without this the library is readable by root, which is not the account
        // Plex runs as.
        for fstype in ["ntfs3", "exfat", "vfat"] {
            let options = options_for(fstype);
            for needed in ["uid=0", "gid=0", "fmask=0133", "dmask=0022"] {
                assert!(
                    options.contains(needed),
                    "{fstype} needs {needed}, got {options}"
                );
            }
        }
        // And the ones that carry their own ownership must not be given the option at
        // all: an option the filesystem does not know is an EINVAL blamed on the drive.
        for fstype in ["ext4", "xfs", "iso9660"] {
            assert!(
                !options_for(fstype).contains("fmask"),
                "{fstype} carries its own ownership"
            );
        }
    }

    #[test]
    fn fat_is_told_to_use_utf8_and_the_others_are_not() {
        // CONFIG_FAT_DEFAULT_IOCHARSET is "iso8859-1", which mangles every non-ASCII
        // filename; exfat and ntfs3 already default to UTF-8. utf8=1 rather than
        // iocharset=utf8, which makes the filesystem case sensitive and is warned about
        // in fs/fat/inode.c:1576.
        assert!(options_for("vfat").contains("utf8=1"));
        assert!(!options_for("vfat").contains("iocharset"));
        assert!(!options_for("exfat").contains("utf8"));
        assert!(!options_for("ntfs3").contains("utf8"));
    }

    #[test]
    fn every_volume_is_mounted_read_only() {
        // Not negotiable, and it is also what lets a hibernated Windows partition be
        // read at all: both of ntfs3's refusals are guarded by !ro.
        for fstype in FILESYSTEMS {
            let options = options_for(fstype);
            assert!(options.starts_with("ro,"), "{fstype}: {options}");
            for flag in ["nosuid", "nodev", "noexec"] {
                assert!(options.contains(flag), "{fstype} needs {flag}: {options}");
            }
        }
    }

    #[test]
    fn every_filesystem_offered_is_one_the_kernel_fragment_builds() {
        // The list here and the CONFIG_* symbols there are two halves of one decision,
        // and nothing else connects them. NTFS is the reason this module exists and was
        // the one symbol the kernel had never been asked for, so this is the assertion
        // that would have caught shipping the feature against a kernel that cannot read
        // a single Windows partition.
        let fragment = include_str!("../../../buildroot/board/plexos/x86_64/linux.fragment");
        for fstype in FILESYSTEMS {
            let symbol = kernel_symbol(fstype)
                .unwrap_or_else(|| panic!("no kernel symbol recorded for {fstype}"));
            assert!(
                fragment.contains(symbol),
                "{fstype} is offered but {symbol} is not in the kernel fragment"
            );
        }
    }

    #[test]
    fn the_kernel_is_asked_for_ntfs_and_asked_for_it_as_a_built_in() {
        // Pinned separately from the loop above, because CONFIG_NTFS3_FS is a *tristate*
        // and CONFIG_MODULES is on. `=m` would pass a build, produce a .ko, and be a
        // filesystem that is silently gone in an image with no modprobe -- which is the
        // eleven-options-became-=m trap pointed at the one filesystem this whole module
        // is about. post-image-test.sh stage 7b asserts the same thing against the
        // .config the kernel was actually built with; this asserts the request.
        let fragment = include_str!("../../../buildroot/board/plexos/x86_64/linux.fragment");
        assert!(fragment.contains("CONFIG_NTFS3_FS=y"));
        assert!(
            !fragment.contains("CONFIG_NTFS3_FS=m"),
            "a module is a filesystem that does not exist in this image"
        );
        // And the compression Windows' own `compact` produces. Without it those files are
        // present, listed, and unreadable, which is the worst of the three states.
        assert!(fragment.contains("CONFIG_NTFS3_LZX_XPRESS=y"));
    }

    /// A volume as the enumerator produces one, before anything has been mounted.
    fn plain(name: &str) -> Volume {
        Volume {
            name: name.to_owned(),
            device: format!("/dev/{name}"),
            disk: "nvme0n1".to_owned(),
            model: "WD PC SN560".to_owned(),
            partuuid: Some("00000000-01".to_owned()),
            label: None,
            bytes: 1_024_209_510_912,
            filesystem: None,
            mounted_at: None,
            readable_by_plex: None,
            refusal: None,
        }
    }

    #[test]
    fn a_drive_that_refused_every_filesystem_does_not_come_back_looking_untried() {
        // The defect the first real scan on hardware found, and it is the one this whole
        // static exists for. `report` derives everything from the machine as it is now,
        // and "was tried and would not mount" is not a state a machine is *in*: the drive
        // is attached, enumerable and unmounted, which is exactly what an untried drive
        // looks like. So the page said "not scanned yet" about a mount that had just
        // failed for a reason the daemon had written down and thrown away.
        let mut volume = plain("nvme0n1p1");
        let mut outcome = ScanOutcome::default();
        outcome.refused.insert(
            "nvme0n1p1".to_owned(),
            "could not be mounted as any of ntfs3, exfat, vfat, ext4, xfs, iso9660".to_owned(),
        );

        carry_over(&mut volume, Some(&outcome));
        assert!(
            volume
                .refusal
                .as_deref()
                .is_some_and(|r| r.contains("ntfs3")),
            "the scan's reason has to survive the response that carried it: {:?}",
            volume.refusal
        );
    }

    #[test]
    fn a_volume_refused_for_what_it_is_keeps_that_reason_rather_than_the_scans() {
        // The running disk is refused before any scan touches it, and that reason is the
        // truer one: it would still be true if nobody had ever pressed the button.
        let mut volume = plain("sda1");
        volume.refusal = Some("this is on sda, the disk MediaLith is running from".to_owned());
        let mut outcome = ScanOutcome::default();
        outcome
            .refused
            .insert("sda1".to_owned(), "would not mount".to_owned());

        carry_over(&mut volume, Some(&outcome));
        assert!(
            volume
                .refusal
                .as_deref()
                .is_some_and(|r| r.contains("running from"))
        );
    }

    #[test]
    fn a_mounted_volume_is_not_given_a_stale_refusal() {
        // A drive that failed one scan and mounted on the next -- somebody formatted it in
        // between, which is exactly what happens after the message tells them to.
        let mut volume = plain("nvme0n1p1");
        volume.mounted_at = Some("/run/plexos/disks/00000000-01".to_owned());
        let mut outcome = ScanOutcome::default();
        outcome
            .refused
            .insert("nvme0n1p1".to_owned(), "would not mount".to_owned());

        carry_over(&mut volume, Some(&outcome));
        assert!(volume.refusal.is_none(), "{:?}", volume.refusal);
    }

    #[test]
    fn with_no_scan_remembered_nothing_is_invented() {
        let mut volume = plain("nvme0n1p1");
        carry_over(&mut volume, None);
        assert!(volume.refusal.is_none());
    }

    #[test]
    fn the_strict_filesystems_are_tried_before_the_loose_one() {
        // vfat will accept things that are not FAT; ntfs3 and exfat check a signature
        // and cannot claim a volume that is not theirs. So the order is a correctness
        // property rather than a guess about what is common.
        let at = |name: &str| FILESYSTEMS.iter().position(|f| *f == name).expect(name);
        assert!(at("ntfs3") < at("vfat"));
        assert!(at("exfat") < at("vfat"));
    }

    #[test]
    fn a_path_outside_the_scan_root_is_refused_however_it_is_spelled() {
        // The canonical path is what is checked, so `..` cannot walk out. Comparing the
        // string as given would accept a path into /var/lib/plexos.
        let error = vet("/etc").expect_err("outside the scan root");
        assert!(matches!(error, Error::NotOnAVolume(_)));
        let error = vet("/run/plexos/disks/../../../etc").expect_err("walked out");
        assert!(matches!(error, Error::NotOnAVolume(_)));
    }

    #[test]
    fn a_scan_path_splits_into_the_volume_and_the_folder_inside_it() {
        let (key, sub) = split_scan_path(Path::new(
            "/run/plexos/disks/b1cf4f2e-9a71-4d0b-8f2c-5d3e7a10c944/Users/seb/Videos",
        ))
        .expect("split");
        assert_eq!(key, "b1cf4f2e-9a71-4d0b-8f2c-5d3e7a10c944");
        assert_eq!(sub, "Users/seb/Videos");

        // The whole volume is a legitimate choice and produces an empty subpath rather
        // than no answer.
        let (key, sub) =
            split_scan_path(Path::new("/run/plexos/disks/6f20736b-01")).expect("split");
        assert_eq!(key, "6f20736b-01");
        assert_eq!(sub, "");

        assert!(split_scan_path(Path::new("/var/media/films")).is_none());
    }

    #[test]
    fn walking_up_stops_at_the_top_of_a_volume() {
        // Otherwise the browser presents the scan directory -- a list of drives -- as
        // though it were a folder on somebody's disk, and the next step up is /run.
        assert_eq!(
            parent_within_scan(Path::new("/run/plexos/disks/abc/Films/2024")).as_deref(),
            Some("/run/plexos/disks/abc/Films")
        );
        assert_eq!(
            parent_within_scan(Path::new("/run/plexos/disks/abc/Films")).as_deref(),
            Some("/run/plexos/disks/abc")
        );
        assert_eq!(parent_within_scan(Path::new("/run/plexos/disks/abc")), None);
        assert_eq!(parent_within_scan(Path::new("/var/media/films")), None);
    }

    #[test]
    fn a_library_name_becomes_a_directory_so_it_is_refused_as_a_shape() {
        let library = |name: &str| Library {
            name: name.to_owned(),
            partuuid: "abc".to_owned(),
            subpath: String::new(),
            filesystem: None,
        };
        assert!(library("films").has_safe_name());
        assert!(library("Films_2024-HD").has_safe_name());
        assert!(!library("").has_safe_name());
        assert!(!library("../etc").has_safe_name());
        assert!(!library("a/b").has_safe_name());
        assert!(!library(&"x".repeat(65)).has_safe_name());
    }

    #[test]
    fn a_library_mounts_under_the_same_root_network_shares_use() {
        let library = Library {
            name: "films".to_owned(),
            partuuid: "abc".to_owned(),
            subpath: "Movies".to_owned(),
            filesystem: Some("ntfs3".to_owned()),
        };
        assert_eq!(library.mount_point(), Path::new("/var/media/films"));
        assert_eq!(ROOT, crate::shares::ROOT);
    }

    #[test]
    fn a_volume_is_mounted_under_its_partuuid_and_not_its_device_name() {
        // Two scans either side of somebody plugging a drive in can renumber sdb to sdc.
        // A browser holding a path built from the device name would then be looking at a
        // different drive with no error anywhere.
        let mut volume = Volume {
            name: "sdb1".to_owned(),
            device: "/dev/sdb1".to_owned(),
            disk: "sdb".to_owned(),
            model: "Elements".to_owned(),
            partuuid: Some("6f20736b-01".to_owned()),
            label: None,
            bytes: 0,
            filesystem: None,
            mounted_at: None,
            readable_by_plex: None,
            refusal: None,
        };
        assert_eq!(
            volume.scan_point(),
            Path::new("/run/plexos/disks/6f20736b-01")
        );
        volume.partuuid = None;
        assert_eq!(volume.scan_point(), Path::new("/run/plexos/disks/sdb1"));
    }

    #[test]
    fn every_refusal_names_a_remedy_or_says_plainly_that_there_is_none() {
        let refusals = [
            Refusal::RunningDisk("sda".to_owned()),
            Refusal::OwnPartition("esp".to_owned()),
            Refusal::SourceUnknown,
            Refusal::Unmountable {
                device: "/dev/sdb1".to_owned(),
                cause: "ntfs3: Invalid argument".to_owned(),
            },
        ];
        for refusal in refusals {
            let said = refusal.to_string();
            assert!(
                said.contains("Remedy:"),
                "a report that stops at the problem has reproduced it: {said}"
            );
        }
    }

    #[test]
    fn every_error_names_a_remedy_or_says_where_the_fault_is() {
        let errors = [
            Error::BadName("../etc".to_owned()),
            Error::Duplicate("films".to_owned()),
            Error::NotOnAVolume("/etc".to_owned()),
            Error::Absent {
                name: "films".to_owned(),
                partuuid: "abc".to_owned(),
            },
            Error::Vanished {
                name: "films".to_owned(),
                path: "/run/plexos/disks/abc/Films".to_owned(),
            },
            Error::Unrememberable {
                volume: "/dev/sdb".to_owned(),
            },
            Error::Mount {
                target: PathBuf::from("/var/media/films"),
                cause: io::Error::from(io::ErrorKind::InvalidInput),
            },
        ];
        for error in errors {
            let said = error.to_string();
            assert!(
                said.contains("Remedy:") || said.contains("fault in MediaLith"),
                "{said}"
            );
        }
    }

    #[test]
    fn an_invalid_mount_option_is_blamed_on_medialith_and_a_missing_device_is_not() {
        // A wrong remedy is worse than none. EINVAL is the option string, which is this
        // program's; ENOENT is a drive somebody pulled out.
        let ours = Error::Mount {
            target: PathBuf::from("/var/media/films"),
            cause: io::Error::from(io::ErrorKind::InvalidInput),
        }
        .to_string();
        assert!(ours.contains("options_for"), "{ours}");

        let theirs = Error::Mount {
            target: PathBuf::from("/var/media/films"),
            cause: io::Error::from(io::ErrorKind::NotFound),
        }
        .to_string();
        assert!(theirs.contains("unplugged"), "{theirs}");
        assert!(!theirs.contains("options_for"), "{theirs}");
    }

    #[test]
    fn an_unmountable_drive_names_bitlocker_and_the_filesystems_this_kernel_lacks() {
        // The two causes somebody actually hits. Neither has a remedy inside MediaLith,
        // which is exactly why the message has to say so rather than suggest retrying.
        let said = Refusal::Unmountable {
            device: "/dev/nvme0n1p3".to_owned(),
            cause: "ntfs3: Invalid argument".to_owned(),
        }
        .to_string();
        assert!(said.contains("BitLocker"), "{said}");
        assert!(said.contains("btrfs"), "{said}");
        for fstype in FILESYSTEMS {
            assert!(said.contains(fstype), "{fstype} missing from: {said}");
        }
    }

    #[test]
    fn media_is_recognised_whatever_case_the_extension_is_written_in() {
        // A Windows disk has .MKV on it as often as .mkv, and a folder reported as
        // holding no media when it holds some is a folder somebody skips past.
        assert!(looks_like_media(Path::new("/x/Film.mkv")));
        assert!(looks_like_media(Path::new("/x/Film.MKV")));
        assert!(looks_like_media(Path::new("/x/Song.FLAC")));
        assert!(!looks_like_media(Path::new("/x/desktop.ini")));
        assert!(!looks_like_media(Path::new("/x/Film")));
    }

    #[test]
    fn readable_by_asks_about_the_account_plex_runs_as_and_not_about_root() {
        // A report that probes as root is answering about the wrong process. This reads
        // the mode bits instead of attempting the access, because root's attempt always
        // succeeds.
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join("plexos-disks-readable-by-uid-900");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("mode");
        assert!(
            readable_by(&dir, PLEX_UID, PLEX_UID),
            "0755 is world-readable"
        );

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
        assert!(
            !readable_by(&dir, PLEX_UID, PLEX_UID),
            "0700 owned by somebody else is not readable by uid 900"
        );

        // A directory needs execute as well as read: r--r--r-- can be listed and not
        // walked into, which is a library Plex cannot use.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o744)).expect("mode");
        assert!(
            !readable_by(&dir, PLEX_UID, PLEX_UID),
            "0744 cannot be walked into"
        );

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browsing_a_directory_lists_folders_and_counts_what_plex_would_play() {
        // The scratch path carries this test's name: Rust runs tests as threads in one
        // process, so a fixed path is a race against whatever else is running.
        let root = std::env::temp_dir().join("plexos-disks-browse-listing");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Movies")).expect("scratch");
        std::fs::create_dir_all(root.join("System Volume Information")).expect("scratch");
        std::fs::write(root.join("Movies/Arrival.mkv"), b"x").expect("scratch");
        std::fs::write(root.join("Movies/Dune.MP4"), b"x").expect("scratch");
        std::fs::write(root.join("Movies/poster.jpg"), b"x").expect("scratch");
        std::fs::write(root.join("System Volume Information/tracking.log"), b"x").expect("scratch");
        std::fs::write(root.join("readme.txt"), b"x").expect("scratch");

        let listing = browse(&root).expect("listed");
        assert_eq!(listing.entries.len(), 2);
        assert!(!listing.truncated);
        assert_eq!(listing.media_here, 0, "readme.txt is not media");

        let movies = listing
            .entries
            .iter()
            .find(|e| e.name == "Movies")
            .expect("Movies");
        assert_eq!(movies.media_here, 2, "one .mkv and one .MP4, not the .jpg");

        let junk = listing
            .entries
            .iter()
            .find(|e| e.name == "System Volume Information")
            .expect("the Windows folder");
        assert_eq!(
            junk.media_here, 0,
            "the count is what tells this apart from Movies at a glance"
        );

        // Sorted case-insensitively, so two listings of one drive do not reorder
        // themselves under somebody's cursor.
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Movies", "System Volume Information"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_state_tells_a_drive_that_is_gone_from_one_that_failed_to_mount() {
        // Two outcomes that both leave nothing at /var/media, and they take opposite
        // remedies: one wants plugging in, the other wants the log reading.
        let state = State {
            library: Library {
                name: "films".to_owned(),
                partuuid: "6f20736b-01".to_owned(),
                subpath: "Films".to_owned(),
                filesystem: Some("ntfs3".to_owned()),
            },
            mount_point: "/var/media/films".to_owned(),
            mounted: false,
            present: true,
        };
        let json = serde_json::to_string(&state).expect("serialised");
        assert!(json.contains("\"present\":true"));
        assert!(json.contains("\"mounted\":false"));
        // Flattened, so the page reads library fields at the top level rather than under
        // a wrapper -- the same shape shares::State has.
        assert!(json.contains("\"name\":\"films\""));
    }
}
