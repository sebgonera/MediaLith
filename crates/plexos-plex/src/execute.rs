//! Carrying out an install plan.
//!
//! Deliberately dull. Every decision worth arguing about lives in
//! [`build::install_plan`](crate::build::install_plan), which is data and is tested;
//! this turns each step into a syscall or a subprocess and reports what happened.
//!
//! # `data.tar.xz` is never written to disk on its own
//!
//! It is 80 MB inside an 83 MB package, on a machine whose only writable filesystem
//! holds the media database. Extracting it to a temporary file, unpacking that, and
//! deleting it costs 80 MB of writes and a window in which a crash leaves a stray file
//! nothing will clean up. Instead the bytes are read straight out of the package at the
//! offset the `ar` directory reported and piped into `tar`, so the payload exists only
//! in flight.
//!
//! # What is verified and what is not
//!
//! The plan's orderings are covered by tests in `build`. This module has been run end
//! to end against a real Plex package **on a build host**, producing a mountable erofs
//! image. It has never run on the appliance. Delete this notice when it has.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::build::Step;
use crate::tools::Tools;

/// How much to move between the package and `tar` at a time.
///
/// 1 MiB: large enough that the syscall overhead disappears against 80 MB, small enough
/// that it is not worth thinking about on a machine with modest memory.
const PIPE_CHUNK: usize = 1024 * 1024;

/// Compression for the app image.
///
/// lz4hc matches what `/usr` uses (`BR2_TARGET_ROOTFS_EROFS_LZ4HC`), and the reason is
/// the same: this image is read constantly once Plex is running, and lz4 decompresses
/// several times faster than lzma at a cost in size that a 5.5 GB `/var` can absorb.
const EROFS_COMPRESSION: &str = "lz4hc";

/// What went wrong, and which step it was.
#[derive(Debug)]
pub struct Failure {
    /// A short description of the step, for the report.
    pub step: String,
    /// What happened.
    pub cause: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.cause)
    }
}

impl std::error::Error for Failure {}

impl Failure {
    fn new(step: &str, cause: impl std::fmt::Display) -> Self {
        Self {
            step: step.to_owned(),
            cause: cause.to_string(),
        }
    }
}

/// Checks that the `mkfs.erofs` in this image can produce the compression an app image
/// uses, before anything is downloaded.
///
/// This exists because the same mistake has now been made twice, in the same shape: a
/// program is present in the image and cannot do the job, while the build host's copy of
/// it can. `busybox tar` could not read `.xz`; the target's `mkfs.erofs` was configured
/// `--disable-lz4` while `post-image.sh` built `/usr` with `lz4hc` through
/// `host-erofs-utils`. Both surfaced minutes into provisioning, after an 83 MB download
/// and a signature verification, as an error about the payload.
///
/// [`Tools::find`](crate::tools::Tools::find) resolves programs up front for the same
/// reason; this goes one step further and asks whether the program can do the thing.
/// `--help` lists the compressors the binary was built with, so the check costs one
/// process and writes nothing.
///
/// # Errors
/// [`Failure`] when the compressor is absent, naming it as an image fault — because that
/// is what it is, and the alternative message blames a package that has not been fetched
/// yet.
pub fn check_compressor(tools: &Tools) -> Result<(), Failure> {
    let output = Command::new(&tools.mkfs_erofs)
        .arg("--help")
        .output()
        .map_err(|e| Failure::new("asking mkfs.erofs what it supports", e))?;

    // Both streams: erofs-utils writes its usage to stderr, and a version that changed
    // its mind about that would silently defeat the check.
    let mut listing = String::from_utf8_lossy(&output.stdout).into_owned();
    listing.push_str(&String::from_utf8_lossy(&output.stderr));

    if listing.lines().any(|line| line.trim() == EROFS_COMPRESSION) {
        return Ok(());
    }

    Err(Failure {
        step: "checking what mkfs.erofs can compress".to_owned(),
        cause: format!(
            "{} was built without the {EROFS_COMPRESSION} compressor, so no app image \
             can be produced. This is an image fault and nothing to do with Plex: set \
             BR2_PACKAGE_EROFS_UTILS_LZ4=y and rebuild. The host's mkfs.erofs is a \
             separate build and having it work there proves nothing about here.",
            tools.mkfs_erofs.display()
        ),
    })
}

/// Runs one step.
///
/// # Errors
/// [`Failure`], naming the step. A failure part-way leaves staging files behind on
/// purpose: they are named so nothing else will read them, and the next attempt clears
/// them itself, so there is no cleanup path here to get wrong under a second failure.
pub fn step(step: &Step, tools: &Tools) -> Result<(), Failure> {
    match step {
        Step::ClearStaging { paths } => {
            for path in paths {
                remove_any(path).map_err(|e| Failure::new("clearing staging", e))?;
            }
            Ok(())
        }

        Step::CreateDir(path) => {
            fs::create_dir_all(path).map_err(|e| Failure::new("creating the unpack directory", e))
        }

        Step::Unpack {
            package,
            offset,
            size,
            into,
        } => unpack(tools, package, *offset, *size, into)
            .map_err(|e| Failure::new("unpacking data.tar.xz", e)),

        Step::Mkfs { from, to } => mkfs(tools, from, to),

        Step::Record {
            image,
            record,
            names,
        } => {
            let digest = sha256(tools, image).map_err(|e| Failure::new("hashing the image", e))?;
            fs::write(record, crate::store::record_body(&digest, names))
                .map_err(|e| Failure::new("writing the integrity record", e))
        }

        Step::Publish { from, to } => {
            fs::rename(from, to).map_err(|e| Failure::new("publishing the image", e))
        }

        Step::Activate { link, target } => activate(link, target),

        Step::Remove { paths } => {
            for path in paths {
                remove_any(path).map_err(|e| Failure::new("removing a superseded image", e))?;
            }
            Ok(())
        }

        Step::RemoveDir(path) => {
            remove_any(path).map_err(|e| Failure::new("removing the unpacked tree", e))
        }
    }
}

/// Runs a whole plan, stopping at the first failure.
///
/// # Errors
/// [`Failure`] from the step that failed.
pub fn plan(steps: &[Step], tools: &Tools, log: &mut dyn FnMut(&str)) -> Result<(), Failure> {
    for (index, one) in steps.iter().enumerate() {
        log(&format!("step {}/{}", index + 1, steps.len()));
        step(one, tools)?;
    }
    Ok(())
}

/// Deletes a file, a directory or a symlink, and is content if it was not there.
fn remove_any(path: &Path) -> io::Result<()> {
    // symlink_metadata, not metadata: `current` is a symlink and may be dangling, and
    // following it would report the target's absence rather than the link's presence.
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

/// Streams `data.tar.xz` out of the package and into `tar`.
fn unpack(tools: &Tools, package: &Path, offset: u64, size: u64, into: &Path) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};

    let mut source = File::open(package)?;
    source.seek(SeekFrom::Start(offset))?;

    // -J for xz. Named explicitly rather than relying on tar sniffing the stream,
    // because busybox tar's autodetection is not guaranteed on a pipe, and the failure
    // is an unhelpful "invalid tar magic" rather than anything about compression.
    let mut child = Command::new(&tools.tar)
        .arg("-xJ")
        .arg("-C")
        .arg(into)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut sink = child.stdin.take().expect("stdin was requested");
    let mut remaining = size;
    let mut buffer = vec![0_u8; PIPE_CHUNK];
    let copied = (|| -> io::Result<()> {
        while remaining > 0 {
            let want = usize::try_from(remaining.min(PIPE_CHUNK as u64)).unwrap_or(PIPE_CHUNK);
            let read = source.read(&mut buffer[..want])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "the package ended {remaining} bytes before data.tar.xz did, so \
                         it is truncated"
                    ),
                ));
            }
            sink.write_all(&buffer[..read])?;
            remaining -= read as u64;
        }
        Ok(())
    })();
    // Dropped before wait, or tar never sees end-of-input and both sides wait forever.
    drop(sink);

    let finished = child.wait_with_output()?;
    copied?;

    if !finished.status.success() {
        return Err(io::Error::other(format!(
            "tar exited {}: {}",
            finished.status,
            String::from_utf8_lossy(&finished.stderr).trim()
        )));
    }
    Ok(())
}

/// Builds the erofs image.
fn mkfs(tools: &Tools, from: &Path, to: &Path) -> Result<(), Failure> {
    let output = Command::new(&tools.mkfs_erofs)
        .arg(format!("-z{EROFS_COMPRESSION}"))
        // Ownership is normalised rather than inherited from whatever unpacked the
        // payload. On the appliance provisioning runs as root and tar restores the
        // package's own root:root, so this changes nothing there — but it means the
        // image does not quietly depend on that being true. Building one as an
        // ordinary user, which is exactly what happens while developing, otherwise
        // produces an image whose every file belongs to uid 1000: a Plex that cannot
        // read its own installation, failing at runtime with permission errors that
        // say nothing about who built the image.
        .arg("--all-root")
        .arg(to)
        .arg(from)
        .output()
        .map_err(|e| Failure::new("running mkfs.erofs", e))?;

    if !output.status.success() {
        return Err(Failure::new(
            "building the app image",
            format!(
                "mkfs.erofs exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Computes a file's SHA256 with the same tool the appliance carries.
fn sha256(tools: &Tools, path: &Path) -> io::Result<String> {
    let output = Command::new(&tools.sha256sum).arg(path).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "sha256sum exited {}",
            output.status
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("sha256sum printed nothing"))
}

/// Points a symlink at a new target without ever unlinking the old one first.
fn activate(link: &Path, target: &str) -> Result<(), Failure> {
    let parent = link.parent().unwrap_or(Path::new("."));
    let scratch = parent.join(".current.swap");

    // A symlink cannot be overwritten in place, and removing it before creating the
    // replacement leaves a window with no `current` at all. Creating a second link and
    // renaming it over the first is atomic, so a crash leaves either the old target or
    // the new one and never nothing.
    let _ = fs::remove_file(&scratch);
    std::os::unix::fs::symlink(target, &scratch)
        .map_err(|e| Failure::new("creating the replacement symlink", e))?;
    fs::rename(&scratch, link).map_err(|e| Failure::new("activating the new image", e))
}

/// Attaches an app image to a free loop device and returns its path.
///
/// `losetup -f --show` picks a device and prints it, which is one command instead of
/// finding a free one and racing another process to claim it.
///
/// # Errors
/// [`Failure`] if `losetup` cannot run, fails, or prints something that is not a device
/// path. The last case is worth distinguishing: a `losetup` that succeeds and prints
/// nothing would otherwise become a mount of the empty string.
pub fn attach_loop(
    tools: &crate::tools::MountTools,
    image: &Path,
) -> Result<std::path::PathBuf, Failure> {
    let output = Command::new(&tools.losetup)
        .arg("-f")
        .arg("--show")
        .arg(image)
        .output()
        .map_err(|e| Failure::new("running losetup", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();

        // The remedy has to match the error. "All the loop devices are in use" is good
        // advice for a busy machine and actively misleading for a losetup that does not
        // understand the flag it was given -- which is what busybox's losetup does, and
        // it sent one diagnosis down entirely the wrong path.
        let remedy = if detail.contains("unrecognized option") || detail.contains("Usage: losetup")
        {
            "This losetup does not understand --show, so it cannot report which device \
             it attached. That is busybox's applet rather than util-linux's, and it is \
             an image fault: set BR2_PACKAGE_UTIL_LINUX_LOSETUP=y and turn busybox's \
             CONFIG_LOSETUP off so exactly one program owns the path."
        } else {
            "All eight of the kernel's pre-created loop devices may be in use — \
             `losetup -a` lists them."
        };

        return Err(Failure::new(
            "attaching the app image to a loop device",
            format!("losetup exited {}: {detail}. {remedy}", output.status),
        ));
    }

    let device = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !device.starts_with("/dev/loop") {
        return Err(Failure::new(
            "attaching the app image to a loop device",
            format!("losetup printed {device:?}, which is not a loop device path"),
        ));
    }
    Ok(std::path::PathBuf::from(device))
}

/// Carries out a mount plan against an image whose bytes have already been checked.
///
/// Returns the loop device, so the caller can detach it if the mount is later undone.
///
/// # Errors
/// [`Failure`] naming the step. A loop device attached before a failed mount is left
/// attached rather than silently detached: `losetup -a` then shows what happened, and
/// unwinding here would mean a second failure path to get wrong during the first.
pub fn mount_plan(
    steps: &[crate::mount::Step],
    tools: &crate::tools::MountTools,
    log: &mut dyn FnMut(&str),
) -> Result<std::path::PathBuf, Failure> {
    let mut device = None;

    for one in steps {
        match one {
            crate::mount::Step::CreateDir(path) => {
                fs::create_dir_all(path)
                    .map_err(|e| Failure::new("creating the Plex mount point", e))?;
            }
            crate::mount::Step::AttachLoop { image } => {
                let attached = attach_loop(tools, image)?;
                log(&format!(
                    "{} attached to {}",
                    image.display(),
                    attached.display()
                ));
                device = Some(attached);
            }
            crate::mount::Step::Mount { target } => {
                let source = device.as_ref().ok_or_else(|| {
                    Failure::new(
                        "mounting the app image",
                        "no loop device was attached; the plan is out of order",
                    )
                })?;
                plexos_sys::mount::mount(
                    &source.to_string_lossy(),
                    &target.to_string_lossy(),
                    crate::mount::FSTYPE,
                    crate::mount::MOUNT_OPTIONS,
                )
                .map_err(|e| Failure::new("mounting the app image", e))?;
                log(&format!("Plex mounted at {}", target.display()));
            }
        }
    }

    device.ok_or_else(|| {
        Failure::new(
            "mounting the app image",
            "the plan attached no loop device, so nothing was mounted",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compression_matches_what_usr_uses() {
        // Not arbitrary: the defconfig sets BR2_TARGET_ROOTFS_EROFS_LZ4HC for /usr, and
        // an app image faulted in constantly wants the same tradeoff.
        let defconfig = include_str!("../../../buildroot/configs/plexos_x86_64_defconfig");
        assert!(defconfig.contains("BR2_TARGET_ROOTFS_EROFS_LZ4HC=y"));
        assert_eq!(EROFS_COMPRESSION, "lz4hc");
    }

    #[test]
    fn a_losetup_that_does_not_understand_the_flag_is_not_blamed_on_busy_loop_devices() {
        // Reported from the appliance: busybox's losetup has no --show, and the message
        // said all eight loop devices might be in use. That is good advice for a busy
        // machine and sends the reader nowhere useful here.
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join("plexos-losetup-remedy");
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("losetup");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf \"losetup: unrecognized option '--show'\\n\" >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let tools = crate::tools::MountTools {
            losetup: stub,
            sha256sum: PathBuf::from("/bin/sha256sum"),
        };
        let message = attach_loop(&tools, Path::new("/nonexistent.img"))
            .unwrap_err()
            .to_string();

        assert!(
            message.contains("BR2_PACKAGE_UTIL_LINUX_LOSETUP"),
            "the remedy must name the option that fixes it: {message}"
        );
        assert!(
            !message.contains("in use"),
            "and must not suggest the loop devices are busy: {message}"
        );
    }

    #[test]
    fn a_mkfs_without_the_compressor_is_refused_before_anything_is_downloaded() {
        use std::path::PathBuf;

        // The check reads a listing rather than creating an image, so it can be tested
        // against a stand-in that prints what the real one prints. The stand-in is a
        // shell script, which is the only part of this that is not the real thing --
        // the parsing being exercised is the parsing that ships.
        let dir = std::env::temp_dir().join("plexos-mkfs-capability");
        std::fs::create_dir_all(&dir).unwrap();

        let write_stub = |name: &str, body: &str| -> PathBuf {
            use std::os::unix::fs::PermissionsExt as _;
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };

        // What the target's mkfs.erofs printed when it had been built --disable-lz4.
        let without = write_stub(
            "mkfs-without",
            "#!/bin/sh\nprintf ' -zX[,level=Y]  X=compressor\\n'\n",
        );
        // And what one built with lz4 prints.
        let with = write_stub(
            "mkfs-with",
            "#!/bin/sh\nprintf ' -zX  supported compressors are:\\n   lz4\\n   lz4hc\\n'\n",
        );

        let tools_with = |mkfs: PathBuf| Tools {
            tar: PathBuf::from("/bin/tar"),
            mkfs_erofs: mkfs,
            sha256sum: PathBuf::from("/bin/sha256sum"),
            sha1sum: PathBuf::from("/bin/sha1sum"),
            losetup: PathBuf::from("/sbin/losetup"),
        };

        let failure = check_compressor(&tools_with(without)).unwrap_err();
        let message = failure.to_string();
        assert!(message.contains("image fault"), "{message}");
        assert!(
            message.contains("BR2_PACKAGE_EROFS_UTILS_LZ4"),
            "and names the option to set: {message}"
        );
        assert!(
            message.contains("nothing to do with Plex"),
            "an image fault and a bad package need opposite responses: {message}"
        );

        assert!(check_compressor(&tools_with(with)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_something_absent_is_not_a_failure() {
        // Every plan begins by clearing staging, and on a first install there is
        // nothing to clear. Treating that as an error would make provisioning fail on
        // exactly the machines it is meant for.
        let missing = std::env::temp_dir().join("plexos-test-definitely-not-here");
        let _ = fs::remove_file(&missing);
        assert!(remove_any(&missing).is_ok());
    }

    #[test]
    fn a_dangling_symlink_is_removed_rather_than_followed() {
        // `current` points at a file that a failed retention pass may already have
        // deleted. metadata() follows the link and reports the target missing, so the
        // link itself would survive and the next activate would trip over it.
        let dir = std::env::temp_dir().join("plexos-test-dangling");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("current");
        std::os::unix::fs::symlink("nowhere.img", &link).unwrap();

        assert!(fs::symlink_metadata(&link).is_ok(), "the link exists");
        assert!(fs::metadata(&link).is_err(), "and it dangles");
        remove_any(&link).unwrap();
        assert!(fs::symlink_metadata(&link).is_err(), "it is gone");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn activating_replaces_an_existing_link_without_a_gap() {
        let dir = std::env::temp_dir().join("plexos-test-activate");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("current");

        activate(&link, "1.42.2.img").unwrap();
        assert_eq!(fs::read_link(&link).unwrap().to_str(), Some("1.42.2.img"));

        // The second call is the one that matters: a symlink cannot be overwritten in
        // place, so a naive implementation fails here with EEXIST.
        activate(&link, "1.43.3.img").unwrap();
        assert_eq!(fs::read_link(&link).unwrap().to_str(), Some("1.43.3.img"));

        assert!(
            !dir.join(".current.swap").exists(),
            "the scratch link is renamed away, not left behind"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_target_stays_relative_through_activation() {
        // An absolute target would bake in where /var was mounted at provisioning time.
        let dir = std::env::temp_dir().join("plexos-test-relative");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("current");
        activate(&link, "1.43.3.img").unwrap();

        let target = fs::read_link(&link).unwrap();
        assert!(target.is_relative(), "{target:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
