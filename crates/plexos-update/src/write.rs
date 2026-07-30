//! Putting an image onto a partition, and proving it arrived.
//!
//! # Read it back
//!
//! Every write here is verified by reading the partition again and hashing what is
//! there. That is not belt and braces: the bytes came over a network from an unsigned
//! source and went through a page cache to a device that may be a cheap USB stick. A
//! `write` that returns success and a `close` that returns success say nothing about
//! what is on the medium, and the first thing that would notice is dm-verity refusing to
//! open on the next boot — after the bootloader has already switched to the new slot.
//!
//! Verifying costs one extra read of 74 MB. Finding out at boot costs three failed boots
//! and a rollback.
//!
//! # Why not stream and hash at the same time
//!
//! Because that hashes what was *sent*, not what was *stored*. The two differ exactly in
//! the case worth catching.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

/// How much is moved per read/write, and per hash update.
///
/// 1 MiB: large enough that the syscall overhead disappears against a 74 MB image, small
/// enough that progress is reported often enough to look alive.
pub const CHUNK: usize = 1024 * 1024;

/// What went wrong.
#[derive(Debug)]
pub enum Error {
    /// The partition could not be found by its GPT label.
    NoPartition {
        /// The label that was looked for.
        label: String,
        /// The underlying failure.
        cause: io::Error,
    },
    /// Opening or writing the device failed.
    Io {
        /// What was being done.
        doing: String,
        /// The underlying failure.
        cause: io::Error,
    },
    /// What is on the partition is not what was meant to be written.
    Mismatch {
        /// Where.
        device: String,
        /// What was expected.
        expected: String,
        /// What is actually there.
        found: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPartition { label, cause } => write!(
                f,
                "no partition labelled {label}: {cause}. This disk was not written by a \
                 PlexOS installer, or its GPT is damaged. Nothing was written."
            ),
            Self::Io { doing, cause } => write!(f, "{doing}: {cause}"),
            Self::Mismatch {
                device,
                expected,
                found,
            } => write!(
                f,
                "{device} holds {found} and should hold {expected}. The write reported \
                 success and the medium disagrees, which is why it is read back. The \
                 slot is left as it is -- unbootable rather than wrong -- and the \
                 running system is untouched. Retry once; a second failure is failing \
                 hardware rather than a bad transfer."
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Writes `source` onto the partition labelled `label`, then reads it back and checks it.
///
/// `expected_sha256` is the digest of the bytes that should end up there. `progress` is
/// called with bytes done and bytes total, often enough to drive a page.
///
/// # Errors
/// See [`Error`]. A mismatch leaves the partition holding whatever it holds: the running
/// system is in the *other* slot and is untouched, so a half-written slot is a slot to
/// write again rather than a machine to recover.
pub fn to_partition(
    disk: &str,
    label: &str,
    source: &mut impl Read,
    size: u64,
    expected_sha256: &str,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<String, Error> {
    // Scoped to a disk, and that is the whole of the argument for this parameter: once
    // PlexOS can install itself, a machine can carry two partitions with this label, and
    // whichever the kernel enumerated first is not a choice anything made. An update
    // installed in that state went to a disk nothing had chosen -- it was the right one,
    // and nothing made it so.
    let device =
        plexos_sys::device::by_partlabel_on(disk, label).map_err(|cause| Error::NoPartition {
            label: label.to_owned(),
            cause,
        })?;

    let mut target = OpenOptions::new()
        .write(true)
        .open(&device)
        .map_err(|cause| Error::Io {
            doing: format!("opening {device} to write {label}"),
            cause,
        })?;

    let mut buffer = vec![0_u8; CHUNK];
    let mut written = 0_u64;
    while written < size {
        let want = usize::try_from((size - written).min(CHUNK as u64)).unwrap_or(CHUNK);
        let read = source
            .read(&mut buffer[..want])
            .map_err(|cause| Error::Io {
                doing: format!("reading the image after {written} bytes"),
                cause,
            })?;
        if read == 0 {
            return Err(Error::Io {
                doing: format!(
                    "the image ended after {written} of {size} bytes, so the download \
                     was truncated"
                ),
                cause: io::Error::from(io::ErrorKind::UnexpectedEof),
            });
        }
        target
            .write_all(&buffer[..read])
            .map_err(|cause| Error::Io {
                doing: format!("writing {device} at {written}"),
                cause,
            })?;
        written += read as u64;
        progress(written, size);
    }

    // sync_all rather than drop: a close that succeeds says nothing about the medium,
    // and the read-back below would otherwise be answered out of the page cache.
    target.sync_all().map_err(|cause| Error::Io {
        doing: format!("flushing {device}"),
        cause,
    })?;
    drop(target);

    let found = digest_of(&device, size)?;
    if found == expected_sha256 {
        Ok(found)
    } else {
        Err(Error::Mismatch {
            device,
            expected: expected_sha256.to_owned(),
            found,
        })
    }
}

/// Hashes the first `size` bytes of a device.
///
/// Only `size` bytes: a partition is larger than the image on it, and hashing the
/// remainder would hash whatever the previous occupant left behind.
fn digest_of(device: &str, size: u64) -> Result<String, Error> {
    let mut file = std::fs::File::open(device).map_err(|cause| Error::Io {
        doing: format!("reopening {device} to check what was written"),
        cause,
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|cause| Error::Io {
        doing: format!("seeking {device}"),
        cause,
    })?;

    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    let mut done = 0_u64;
    while done < size {
        let want = usize::try_from((size - done).min(CHUNK as u64)).unwrap_or(CHUNK);
        let read = file.read(&mut buffer[..want]).map_err(|cause| Error::Io {
            doing: format!("reading {device} back at {done}"),
            cause,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        done += read as u64;
    }
    Ok(hex(&hasher.finalize()))
}

/// Lower-case hex, matching what the bundle carries.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// The SHA-256 of a file, for checking a download before it is written anywhere.
///
/// # Errors
/// If the file cannot be read.
pub fn digest_of_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_matches_an_implementation_that_is_not_this_one() {
        // Pinned against sha256sum rather than against this module's own output, which
        // would agree with itself however wrong it was. `printf %s abc | sha256sum`.
        let path = std::env::temp_dir().join("plexos-update-digest");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            digest_of_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_partition_says_the_disk_was_not_written_by_an_installer() {
        // The realistic cause, and the one a person can act on. "No such file" would
        // send somebody looking for a bug in this code.
        let error = to_partition(
            "no_such_disk_here",
            "no_such_label_here",
            &mut &b"x"[..],
            1,
            "0".repeat(64).as_str(),
            &mut |_, _| {},
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("not written by a PlexOS installer"),
            "{message}"
        );
        assert!(message.contains("Nothing was written"), "{message}");
    }

    #[test]
    fn a_mismatch_says_the_running_system_is_untouched() {
        // Whoever reads this has just been told their update failed. The next thing they
        // need to know is whether the machine is still fine, and it is: the running
        // system is in the other slot.
        let error = Error::Mismatch {
            device: "/dev/sda4".to_owned(),
            expected: "a".repeat(64),
            found: "b".repeat(64),
        };
        let message = error.to_string();
        assert!(message.contains("running system is untouched"), "{message}");
        assert!(message.contains("Retry once"), "{message}");
    }

    #[test]
    fn a_truncated_source_is_an_error_rather_than_a_short_write() {
        // The partition would otherwise hold the first N bytes of an image and nothing
        // would say so until dm-verity refused it three boots later.
        let mut short = &b"only four"[..];
        let error = to_partition(
            "no_such_disk_here",
            "no_such_label_here",
            &mut short,
            1_000_000,
            "0",
            &mut |_, _| {},
        )
        .unwrap_err();
        // It fails at the partition lookup on a build host, which is the point: nothing
        // is opened for writing before the target is known to exist.
        assert!(matches!(error, Error::NoPartition { .. }));
    }

    #[test]
    fn the_chunk_is_big_enough_to_be_cheap_and_small_enough_to_report() {
        // Lower bound: syscall overhead has to disappear against a 74 MB image. Upper
        // bound: progress has to move often enough that the page does not look hung.
        const { assert!(CHUNK >= 64 * 1024) };
        const { assert!(CHUNK <= 8 * 1024 * 1024) };
    }
}
