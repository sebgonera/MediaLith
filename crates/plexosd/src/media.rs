//! Reading a Plex package off removable media, for a machine with no way to fetch one.
//!
//! The other half of ADR-0010's offline route. [`crate::console::upload_route`] takes a
//! package from a browser; this takes one from a USB stick, which is what somebody with
//! no browser on the same network actually has. Both end in the same place — a file at
//! `apps/.package.deb` and [`crate::provision::Source::Supplied`] — so neither has its own
//! path through the signature checks.
//!
//! # What it will mount
//!
//! [`FILESYSTEMS`], in order, each read-only. The list is not a guess: it is what the
//! kernel fragment builds, and **exFAT had to be added to it for this to be useful at
//! all**. Windows formats anything above 32 GB as exFAT and offers no FAT32 option, so
//! "put the package on a stick" means exFAT on most sticks a person owns. Without the
//! kernel support the mount fails with `EINVAL`, an error about *arguments* that says
//! nothing about filesystems — the same disguise `CONFIG_NFS_V4_1` wore for four build
//! cycles.
//!
//! # What it will not do
//!
//! Read anything that is not on the media. A chosen path is checked to be under
//! [`MOUNT_ROOT`] and to end in `.deb` before it is opened, because the alternative is a
//! route that hands back any file on the appliance to whoever names it. The token is
//! required either way; that is not a reason to leave the hole.
//!
//! It does not write to the media, ever. Mounts are read-only, so a stick that is
//! somebody's only copy of something cannot be damaged by being read here.
//!
//! # What has run
//!
//! **Nothing here has run on a machine.** The pure parts are tested; mounting needs root
//! and a device.

use std::io;
use std::path::{Path, PathBuf};

use plexos_gpu::env::Environment;

/// Where media is mounted while it is being read.
///
/// Under `/run` because it is a tmpfs the running root already has, and because nothing
/// here should survive a reboot: a mount left behind would hold a device somebody has
/// physically removed.
pub const MOUNT_ROOT: &str = "/run/plexos/media";

/// Filesystems tried, in order, and the reason the order is this one.
///
/// vfat and exfat first because a removable stick is one of the two far more often than
/// anything else; `ntfs3` next, which arrived with [`crate::disks`] and covers a stick
/// somebody has been using with Windows; ext4 for one formatted on a Linux machine, which
/// is likely for anybody handling a Debian package; then iso9660 for a disc or a copied
/// image. Each is a `CONFIG_*` symbol in the kernel fragment, and a name here that the
/// kernel cannot mount produces a failure blamed on the medium rather than on the image.
///
/// The order differs from [`crate::disks::FILESYSTEMS`] deliberately — there the case is
/// an internal Windows disk and `ntfs3` leads — and the difference is safe rather than
/// merely tolerable. `vfat` cannot claim an NTFS volume even though NTFS deliberately
/// gives its boot sector a FAT-shaped BPB: NTFS writes zero for both the reserved-sector
/// count and the number of FATs, and `fat_read_bpb` rejects each
/// (`fs/fat/inode.c:1417,1423`). The archaic no-BPB fallback below that needs the whole
/// BPB region to be zero *and* the device to be a recognised floppy size, which no
/// Windows drive is. So probing order here is about how fast the right answer is found,
/// not about which answer is found.
pub const FILESYSTEMS: [&str; 6] = ["vfat", "exfat", "ntfs3", "ext4", "iso9660", "xfs"];

/// How deep the search goes.
///
/// The top level and one directory below it. A person drops a download at the root of a
/// stick or into `Downloads/`, and walking a whole filesystem to find a file somebody
/// could have named would be slow on the medium where it is slowest.
pub const SEARCH_DEPTH: usize = 1;

/// Where `/sys` lists block devices.
const SYS_BLOCK: &str = "/sys/block";

/// A partition that might carry a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Kernel name, e.g. `sdb1`.
    pub name: String,
    /// The device node to mount.
    pub device: String,
    /// Size in bytes, for telling two sticks apart on the page.
    pub bytes: u64,
}

/// A package found on a volume.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Found {
    /// The volume it was on, by kernel name.
    pub volume: String,
    /// Full path while the medium is mounted.
    pub path: String,
    /// What to show: the file name alone.
    pub name: String,
    /// Size in bytes.
    pub bytes: u64,
}

/// Partitions worth trying, newest-looking first is not attempted — order is sysfs order.
///
/// The running disk is excluded. Its partitions are this appliance's own `/usr` and
/// `/var`, and offering them would invite somebody to install a package the machine is
/// already running from. `running` comes from [`crate::install::running_disk`], which
/// resolves it through dm-verity's `slaves` rather than by trusting a label — the same
/// lookup that fixed the two-disk ambiguity.
///
/// # Errors
/// If `/sys/block` cannot be read at all. One unreadable disk is skipped rather than
/// failing the scan, because a machine with a flaky drive must still be able to install.
pub fn volumes(env: &impl Environment, running: Option<&str>) -> io::Result<Vec<Volume>> {
    let mut found = Vec::new();
    for disk in env.list_dir(Path::new(SYS_BLOCK))? {
        let Some(name) = disk.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Loop and device-mapper devices are not media somebody plugged in, and the
        // running root is made of them.
        if name.starts_with("loop") || name.starts_with("dm-") || name.starts_with("ram") {
            continue;
        }
        if running.is_some_and(|r| r == name) {
            continue;
        }

        let Ok(entries) = env.list_dir(&disk) else {
            continue;
        };
        for entry in entries {
            let Some(part) = entry.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !part.starts_with(name) || part == name {
                continue;
            }
            let Some(bytes) = env
                .read(&entry.join("size"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|sectors| sectors * 512)
            else {
                continue;
            };
            found.push(Volume {
                name: part.to_owned(),
                device: format!("/dev/{part}"),
                bytes,
            });
        }
    }
    Ok(found)
}

/// Whether a file is a Debian package by name.
///
/// By name because that is all that can be known before it is opened, and opening it is
/// [`crate::provision`]'s job — where a file that is not a package fails the `ar`
/// directory read with a message about the package rather than about the medium.
#[must_use]
pub fn looks_like_a_package(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("deb"))
}

/// Packages at the top of `dir` and one level below it.
///
/// Reads the real filesystem rather than going through [`Environment`], unlike
/// [`volumes`]. That abstraction exists so sysfs can be faked in a test; this walks a
/// filesystem that only exists once something has been mounted, and a fixture of it would
/// describe a machine where the mount had already succeeded — which is the shape of test
/// this project has been burned by before.
///
/// Sorted, so two scans of the same stick produce the same list and the page does not
/// reorder itself under somebody's cursor.
#[must_use]
pub fn packages_in(dir: &Path, volume: &str) -> Vec<Found> {
    let mut found = Vec::new();
    collect(dir, volume, 0, &mut found);
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

fn collect(dir: &Path, volume: &str, depth: usize, out: &mut Vec<Found>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // Symlinks are not followed. A link on the medium could point anywhere on the
        // appliance, and the vetting below canonicalises -- so a followed link would be
        // offered and then correctly refused, which reads as the console being broken.
        if kind.is_file() && looks_like_a_package(&path) {
            out.push(Found {
                volume: volume.to_owned(),
                path: path.to_string_lossy().into_owned(),
                name: entry.file_name().to_string_lossy().into_owned(),
                bytes: entry.metadata().map(|m| m.len()).unwrap_or(0),
            });
        } else if kind.is_dir() && depth < SEARCH_DEPTH {
            collect(&path, volume, depth + 1, out);
        }
    }
}

/// Whether a path the console was given may be opened as a package.
///
/// Two conditions, and both matter. It has to be **under [`MOUNT_ROOT`]**, or this route
/// reads any file on the appliance to whoever names one — `/var/lib/plexos/device-token`
/// included, which would turn an offline convenience into a way past the credential it
/// requires. And it has to end in `.deb`, which is not security but is the difference
/// between a clear refusal and a signature error about a file that was never a package.
///
/// The check is on the *canonical* path, so `..` cannot walk out of the mount. Comparing
/// the string as given would accept
/// `/run/plexos/media/sdb1/../../../var/lib/plexos/device-token`.
#[must_use]
pub fn is_on_media(canonical: &Path) -> bool {
    canonical.starts_with(MOUNT_ROOT) && looks_like_a_package(canonical)
}

/// Why a chosen package was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The path is not on mounted media, or is not a `.deb`.
    NotOnMedia(String),
    /// It could not be resolved at all: removed between the scan and the choice.
    Gone(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOnMedia(path) => write!(
                f,
                "{path} is not a package on mounted removable media. Only files under \
                 {MOUNT_ROOT} ending in .deb may be installed this way — choose one from \
                 the list rather than typing a path."
            ),
            Self::Gone(path) => write!(
                f,
                "{path} could not be opened. The medium may have been unplugged since it \
                 was scanned; plug it back in and scan again."
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Resolves a chosen path and refuses anything that is not a package on the media.
///
/// # Errors
/// See [`Refusal`].
pub fn vet(chosen: &str) -> Result<PathBuf, Refusal> {
    let canonical = std::fs::canonicalize(chosen).map_err(|_| Refusal::Gone(chosen.to_owned()))?;
    if is_on_media(&canonical) {
        Ok(canonical)
    } else {
        Err(Refusal::NotOnMedia(chosen.to_owned()))
    }
}

/// Mounts a volume read-only, trying each filesystem in turn.
///
/// Returns which one worked, so a failure can say what was tried rather than "mount
/// failed". Read-only always: a stick may be somebody's only copy, and nothing here needs
/// to write.
///
/// # Errors
/// If none of [`FILESYSTEMS`] mounts it. The message names them all, because the usual
/// cause is a filesystem this kernel does not build and the remedy is a different stick
/// rather than a different attempt.
pub fn mount_ro(device: &str, target: &Path) -> io::Result<&'static str> {
    std::fs::create_dir_all(target)?;
    let mut last = String::new();
    for fstype in FILESYSTEMS {
        match plexos_sys::mount::mount(
            device,
            &target.to_string_lossy(),
            fstype,
            "ro,nodev,nosuid,noexec",
        ) {
            Ok(()) => return Ok(fstype),
            Err(error) => last = error.to_string(),
        }
    }
    let _ = std::fs::remove_dir(target);
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{device} could not be mounted as any of {}: {last}. FAT32, exFAT and NTFS \
             are all read here, so a stick formatted by Windows should have worked \
             whatever size it is. Remedy: if this is a drive BitLocker is encrypting, \
             nothing in MediaLith can open it — unlock it in Windows and turn BitLocker \
             off for that drive. Otherwise copy the package to another stick, or send it \
             from a browser instead.",
            FILESYSTEMS.join(", ")
        ),
    ))
}

/// Unmounts and removes the directory, reporting nothing.
///
/// Failure here is deliberately quiet. It happens when the medium was pulled, which is
/// not a fault worth a message on a page about installing Plex, and the mount goes with
/// the tmpfs at the next boot regardless.
pub fn unmount(target: &Path) {
    let _ = plexos_sys::mount::unmount(&target.to_string_lossy());
    let _ = std::fs::remove_dir(target);
}

/// Where a volume is mounted while it is read.
#[must_use]
pub fn mount_point(volume: &str) -> PathBuf {
    Path::new(MOUNT_ROOT).join(volume)
}

/// What a scan found, and what it could not look at.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Scan {
    /// Packages found, across every volume that mounted.
    pub packages: Vec<Found>,
    /// Volumes that could not be mounted, with the reason.
    ///
    /// Reported rather than dropped. A stick that is present and unreadable is the case
    /// somebody needs told about — silence here reads as "no package on it", which sends
    /// them looking for the file instead of at the filesystem.
    pub skipped: Vec<String>,
}

/// Mounts every candidate volume, looks for packages, and unmounts.
///
/// Left mounted only for the duration: a medium held open is one somebody cannot pull,
/// and the path handed back is re-mounted when it is chosen. That costs a second mount
/// and buys a console that does not pin hardware between two clicks.
///
/// Note that the paths in [`Scan::packages`] are therefore **not valid when the scan
/// returns**. They are identifiers to choose from, and [`fetch`] mounts again before
/// opening one. Anything else would be a path that works until the medium is touched.
pub fn scan(env: &impl Environment, running: Option<&str>) -> Scan {
    let mut scan = Scan::default();
    let Ok(volumes) = volumes(env, running) else {
        scan.skipped.push(format!(
            "{SYS_BLOCK} could not be read, so no media could be found"
        ));
        return scan;
    };

    for volume in volumes {
        let point = mount_point(&volume.name);
        match mount_ro(&volume.device, &point) {
            Ok(_) => {
                scan.packages.extend(packages_in(&point, &volume.name));
                unmount(&point);
            }
            // Most volumes on most machines are not media with a package on them, and a
            // page listing every EFI partition that failed to mount as five filesystems
            // would bury the one line that matters. Only a failure that is *not* simply
            // "nothing here understands this" is worth reporting.
            Err(error) if error.kind() != io::ErrorKind::InvalidData => {
                scan.skipped.push(format!("{}: {error}", volume.name));
            }
            Err(_) => {}
        }
    }
    scan
}

/// Copies a chosen package into place, mounting its medium to do so.
///
/// Returns how many bytes were copied. The destination is the same file the download
/// writes and the upload streams to, so provisioning sees one thing whatever put it
/// there.
///
/// # Errors
/// If the path is not a package on media ([`vet`]), if the medium cannot be mounted, or
/// if the copy fails.
pub fn fetch(env: &impl Environment, chosen: &str, destination: &Path) -> io::Result<u64> {
    let volume = Path::new(chosen)
        .strip_prefix(MOUNT_ROOT)
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                Refusal::NotOnMedia(chosen.to_owned()).to_string(),
            )
        })?;

    let device = volumes(env, None)?
        .into_iter()
        .find(|v| v.name == volume)
        .map(|v| v.device)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{volume} is no longer attached. The medium was unplugged since it \
                     was scanned; plug it back in and scan again."
                ),
            )
        })?;

    let point = mount_point(&volume);
    mount_ro(&device, &point)?;

    // Vetted *after* the mount, because canonicalising a path on an unmounted directory
    // resolves to nothing and every choice would be refused as gone.
    let outcome = vet(chosen)
        .map_err(|refusal| io::Error::new(io::ErrorKind::InvalidInput, refusal.to_string()))
        .and_then(|source| {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source, destination)
        });

    unmount(&point);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_debs_under_the_mount_root_are_accepted() {
        assert!(is_on_media(Path::new(
            "/run/plexos/media/sdb1/plexmediaserver_1.43_amd64.deb"
        )));
        assert!(is_on_media(Path::new(
            "/run/plexos/media/sdb1/Downloads/plex.DEB"
        )));
    }

    #[test]
    fn nothing_off_the_media_is_accepted_however_it_is_spelled() {
        // The one that matters: this route is given a path by whoever holds the token,
        // and a check on the string as typed would let `..` walk out of the mount to the
        // credential file -- turning an offline convenience into a way past the very
        // credential it asks for. vet() canonicalises first; this asserts the predicate
        // it then applies.
        for path in [
            "/var/lib/plexos/device-token",
            "/etc/shadow",
            "/run/plexos/media/../../var/lib/plexos/device-token",
            "/run/plexos/mediaX/evil.deb",
        ] {
            assert!(!is_on_media(Path::new(path)), "accepted {path}");
        }
    }

    #[test]
    fn a_file_on_the_media_that_is_not_a_package_is_refused() {
        // Not security -- provisioning would reject it a moment later -- but the
        // difference between "that is not a package" and a signature error about a file
        // that never was one.
        assert!(!is_on_media(Path::new(
            "/run/plexos/media/sdb1/holiday.jpg"
        )));
        assert!(!is_on_media(Path::new(
            "/run/plexos/media/sdb1/plex.deb.txt"
        )));
    }

    #[test]
    fn the_extension_is_matched_without_regard_to_case() {
        // A stick written by Windows may well carry PLEXMEDIASERVER.DEB.
        assert!(looks_like_a_package(Path::new("a.deb")));
        assert!(looks_like_a_package(Path::new("A.DEB")));
        assert!(looks_like_a_package(Path::new("a.Deb")));
        assert!(!looks_like_a_package(Path::new("a.debian")));
        assert!(!looks_like_a_package(Path::new("deb")));
    }

    #[test]
    fn every_filesystem_offered_is_one_the_kernel_fragment_builds() {
        // The list here and the CONFIG_* symbols there are two halves of one decision,
        // and nothing else connects them. A name added here without the symbol produces
        // a mount that fails with EINVAL, which reads as a broken stick.
        //
        // The mapping lives in `disks` and not here, because that module offers an
        // overlapping list for a different purpose and two copies of one table is one
        // copy that goes stale.
        let fragment = include_str!("../../../buildroot/board/plexos/x86_64/linux.fragment");
        for fstype in FILESYSTEMS {
            let symbol = crate::disks::kernel_symbol(fstype)
                .unwrap_or_else(|| panic!("no kernel symbol recorded for {fstype}"));
            assert!(
                fragment.contains(symbol),
                "{fstype} is offered but {symbol} is not in the kernel fragment"
            );
        }
    }

    #[test]
    fn exfat_is_offered_because_most_sticks_are_exfat() {
        // Recorded as its own test because it is the one somebody would remove as
        // redundant. Windows gives no FAT32 option above 32 GB, so dropping exfat makes
        // the removable-media path fail on the majority of sticks a person owns.
        assert!(FILESYSTEMS.contains(&"exfat"));
    }

    #[test]
    fn the_mount_point_is_under_the_root_the_check_enforces() {
        let point = mount_point("sdb1");
        assert!(point.starts_with(MOUNT_ROOT));
        assert!(is_on_media(&point.join("plex.deb")));
    }
}
