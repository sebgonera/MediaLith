//! Creating a device-mapper verity target, without `veritysetup`.
//!
//! `veritysetup` lives in `cryptsetup`, which is installed into the target `/usr` —
//! behind the very verity device it would be needed to create. The initrd is one
//! static binary with no loader, so shipping `veritysetup` there would mean shipping
//! `libcryptsetup`, `libdevmapper` and their dependencies with it. Talking to
//! `/dev/mapper/control` directly is a few hundred lines and keeps the initrd what
//! ARCHITECTURE.md §3 says it is.
//!
//! # The sequence
//!
//! Creating a live target takes three ioctls, and the order is the kernel's:
//!
//! 1. `DM_DEV_CREATE` — a named device with no table. Returns its `dev_t`.
//! 2. `DM_TABLE_LOAD` — the verity target, loaded as the *inactive* table.
//! 3. `DM_DEV_SUSPEND` without `DM_SUSPEND_FLAG` — a resume, which swaps the
//!    inactive table in and makes the device readable.
//!
//! Stopping after step 2 leaves a device that exists and returns `EIO` on every read,
//! which is a confusing way to fail.
//!
//! # Why this also makes the device node
//!
//! On a normal system `udev` creates `/dev/mapper/<name>` in response to a uevent.
//! The initrd has no `udev` — that is the point of it — so `devtmpfs` gives us
//! `/dev/dm-N` and nothing else. The boot plan mounts `/dev/mapper/plexos-usr`, so
//! this module creates that node itself from the `dev_t` the kernel returned. Without
//! it the mount fails with `ENOENT` on a device that exists and works.
//!
//! # Provenance of the constants below
//!
//! Structure sizes, field offsets and command numbers are an ABI, not a Rust struct,
//! so the compiler cannot check any of them and a wrong value yields `ENOTTY` or
//! `EINVAL` with nothing to say which field was at fault. They were therefore taken
//! from `linux/dm-ioctl.h` by compiling a C program against it and printing
//! `sizeof` and `offsetof`, rather than written from memory:
//!
//! ```text
//! sizeof(struct dm_ioctl)       312      offsetof(dm_ioctl, data_size)    12
//! sizeof(struct dm_target_spec)  40      offsetof(dm_ioctl, data_start)   16
//! DM_DEV_CREATE_CMD               3      offsetof(dm_ioctl, target_count) 20
//! DM_DEV_SUSPEND_CMD              6      offsetof(dm_ioctl, flags)        28
//! DM_TABLE_LOAD_CMD               9      offsetof(dm_ioctl, dev)          40
//! DM_DEV_CREATE          0xc138fd03      offsetof(dm_ioctl, name)         48
//! DM_DEV_SUSPEND         0xc138fd06      offsetof(dm_target_spec, length)  8
//! DM_TABLE_LOAD          0xc138fd09      offsetof(dm_target_spec, next)   20
//!                                        offsetof(dm_target_spec, type)   24
//! ```
//!
//! The tests below pin the computed request numbers against those three values.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::verity::VeritySuperblock;

/// The device-mapper control node.
pub const CONTROL: &str = "/dev/mapper/control";

/// Directory in which named device-mapper nodes appear.
pub const MAPPER_DIR: &str = "/dev/mapper";

/// `struct dm_ioctl` is this many bytes, and the ioctl request number encodes it.
/// Wrong here means `ENOTTY` from every call.
const DM_IOCTL_SIZE: usize = 312;

/// The same value as a `u32`, which is what the ABI fields and the request encoding
/// want. Kept as a separate constant rather than cast at each use: these are wire
/// fields, and a lossy cast on this path is worth making impossible rather than
/// merely unlikely.
const DM_IOCTL_SIZE_U32: u32 = 312;
const _: () = assert!(DM_IOCTL_SIZE == DM_IOCTL_SIZE_U32 as usize);

/// `struct dm_target_spec`.
const DM_TARGET_SPEC_SIZE: usize = 40;

/// `DM_NAME_LEN` from `linux/dm-ioctl.h`.
const DM_NAME_LEN: usize = 128;

/// Offset of the `name` field within `struct dm_ioctl`.
const NAME_OFFSET: usize = 48;

/// The interface version this code speaks. The kernel rejects a major mismatch.
const DM_VERSION: [u32; 3] = [4, 0, 0];

const DM_IOCTL_TYPE: u32 = 0xfd;
const DM_DEV_CREATE_CMD: u32 = 3;
const DM_TABLE_LOAD_CMD: u32 = 9;
const DM_DEV_SUSPEND_CMD: u32 = 6;

/// `DM_READONLY_FLAG`. A verity device is read-only by construction; asking for it
/// writable succeeds and then fails on the first write, far from the cause.
const DM_READONLY_FLAG: u32 = 1;

/// Builds an `_IOWR(DM_IOCTL_TYPE, cmd, struct dm_ioctl)` request number.
///
/// The encoding is the kernel's `_IOC` macro: direction in the top two bits, then
/// the size of the argument, then the type, then the command.
const fn iowr(cmd: u32) -> u64 {
    const READ_WRITE: u32 = 3;
    ((READ_WRITE << 30) | (DM_IOCTL_SIZE_U32 << 16) | (DM_IOCTL_TYPE << 8) | cmd) as u64
}

/// Rounds up to the 8-byte alignment the kernel expects between target specs.
const fn align8(value: usize) -> usize {
    value.div_ceil(8) * 8
}

/// Fills the fixed part of a `struct dm_ioctl` at the start of `buffer`.
///
/// Separated out and tested because getting a field offset wrong produces `EINVAL`
/// with no indication of which field, and the offsets cannot be checked by the
/// compiler — they are an ABI, not a Rust struct.
fn write_header(buffer: &mut [u8], name: &str, data_size: u32, target_count: u32, flags: u32) {
    buffer[0..4].copy_from_slice(&DM_VERSION[0].to_ne_bytes());
    buffer[4..8].copy_from_slice(&DM_VERSION[1].to_ne_bytes());
    buffer[8..12].copy_from_slice(&DM_VERSION[2].to_ne_bytes());
    buffer[12..16].copy_from_slice(&data_size.to_ne_bytes());
    buffer[16..20].copy_from_slice(&DM_IOCTL_SIZE_U32.to_ne_bytes());
    buffer[20..24].copy_from_slice(&target_count.to_ne_bytes());
    buffer[28..32].copy_from_slice(&flags.to_ne_bytes());

    let name_bytes = name.as_bytes();
    buffer[NAME_OFFSET..NAME_OFFSET + name_bytes.len()].copy_from_slice(name_bytes);
}

/// Builds the complete `DM_TABLE_LOAD` payload: header, one target spec, parameters.
///
/// Pure, so the byte layout can be asserted without a device-mapper device or root.
fn table_load_buffer(name: &str, sectors: u64, target_type: &str, params: &str) -> Vec<u8> {
    // header | target spec | NUL-terminated params, padded to 8
    let params_offset = DM_IOCTL_SIZE + DM_TARGET_SPEC_SIZE;
    let params_len = align8(params.len() + 1);
    let total = params_offset + params_len;

    let mut buffer = vec![0u8; total];
    write_header(
        &mut buffer,
        name,
        u32::try_from(total).unwrap_or(u32::MAX),
        1,
        DM_READONLY_FLAG,
    );

    let spec = DM_IOCTL_SIZE;
    buffer[spec..spec + 8].copy_from_slice(&0u64.to_ne_bytes()); // sector_start
    buffer[spec + 8..spec + 16].copy_from_slice(&sectors.to_ne_bytes()); // length
    // `next` is the offset from this spec to the following one. With a single target
    // it is the total length of this spec plus its parameters, and the kernel uses it
    // to find the end of the table.
    let next = u32::try_from(DM_TARGET_SPEC_SIZE + params_len).unwrap_or(u32::MAX);
    buffer[spec + 20..spec + 24].copy_from_slice(&next.to_ne_bytes());

    let type_bytes = target_type.as_bytes();
    buffer[spec + 24..spec + 24 + type_bytes.len()].copy_from_slice(type_bytes);

    buffer[params_offset..params_offset + params.len()].copy_from_slice(params.as_bytes());
    buffer
}

/// Builds the payload for an ioctl that carries no table.
fn header_only_buffer(name: &str) -> Vec<u8> {
    let mut buffer = vec![0u8; DM_IOCTL_SIZE];
    write_header(&mut buffer, name, DM_IOCTL_SIZE_U32, 0, 0);
    buffer
}

/// Reads the `dev` field the kernel filled in, as a `(major, minor)` pair.
fn device_numbers(buffer: &[u8]) -> (u32, u32) {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&buffer[40..48]);
    let dev = u64::from_ne_bytes(raw);
    // The kernel's dm_ioctl.dev is a 32-bit encoding: major in bits 8..20, minor in
    // the low 8 bits plus the high bits above 20.
    // The low 32 bits are the whole of the encoding; discarding the rest is the
    // format, not a lossy cast, so mask explicitly and convert infallibly.
    let low = u32::try_from(dev & 0xffff_ffff).unwrap_or(0);
    (
        ((low >> 8) & 0xfff),
        (low & 0xff) | ((low >> 12) & 0xfff_ff00),
    )
}

fn ioctl(control: &File, request: u64, buffer: &mut [u8]) -> io::Result<()> {
    // SAFETY: `buffer` is at least DM_IOCTL_SIZE bytes (every caller allocates that
    // much or more) and is the layout the request number declares, so the kernel
    // reads and writes only within it. The fd is owned by `control` and outlives the
    // call. The pointer is valid for the duration of the call and is not retained.
    let result = unsafe {
        libc::ioctl(
            control.as_raw_fd(),
            request,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Creates `/dev/mapper/<name>` as a block device node with the given numbers.
fn make_node(path: &Path, major: u32, minor: u32) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "device path contains a NUL"))?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call. The
    // mode and device number are plain integers. mknod does not retain the pointer.
    let result = unsafe {
        libc::mknod(
            c_path.as_ptr(),
            libc::S_IFBLK | 0o600,
            libc::makedev(major, minor),
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Ok(())
}

/// Sets up a read-only dm-verity device and returns the path to its node.
///
/// Every block read through the returned device is checked against the Merkle tree
/// for as long as the device exists — not once at mount time (ADR-0004).
///
/// # Errors
///
/// Any ioctl failure. A failure here is fatal to the boot by design: falling back to
/// an unverified mount would defeat the entire trust chain, so the caller must fail
/// the slot rather than continue.
pub fn create_verity(
    name: &str,
    data_device: &str,
    hash_device: &str,
    root_hash: &str,
    superblock: &VeritySuperblock,
) -> io::Result<PathBuf> {
    if name.len() >= DM_NAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "device-mapper name {name:?} is too long (max {})",
                DM_NAME_LEN - 1
            ),
        ));
    }

    let control = File::options().read(true).write(true).open(CONTROL)?;

    let mut create = header_only_buffer(name);
    ioctl(&control, iowr(DM_DEV_CREATE_CMD), &mut create)?;
    let (major, minor) = device_numbers(&create);

    let params = superblock.table_line(data_device, hash_device, root_hash);
    let mut load = table_load_buffer(name, superblock.sectors(), "verity", &params);
    ioctl(&control, iowr(DM_TABLE_LOAD_CMD), &mut load)?;

    // A resume: DM_DEV_SUSPEND with DM_SUSPEND_FLAG clear swaps the inactive table in.
    let mut resume = header_only_buffer(name);
    ioctl(&control, iowr(DM_DEV_SUSPEND_CMD), &mut resume)?;

    let path = Path::new(MAPPER_DIR).join(name);
    make_node(&path, major, minor)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn superblock() -> VeritySuperblock {
        VeritySuperblock::parse(include_bytes!(
            "../tests/fixtures/verity-superblock-sha256.bin"
        ))
        .unwrap()
    }

    #[test]
    fn request_numbers_match_the_kernel_header() {
        // Not computed here and compared to itself, which would prove nothing. These
        // three values were printed by a C program compiled against
        // linux/dm-ioctl.h; see the module documentation. If the encoding in iowr()
        // is wrong in any way — size, type, direction — none of them will match.
        assert_eq!(iowr(DM_DEV_CREATE_CMD), 0xc138_fd03, "DM_DEV_CREATE");
        assert_eq!(iowr(DM_DEV_SUSPEND_CMD), 0xc138_fd06, "DM_DEV_SUSPEND");
        assert_eq!(iowr(DM_TABLE_LOAD_CMD), 0xc138_fd09, "DM_TABLE_LOAD");
    }

    #[test]
    fn the_structure_sizes_are_the_ones_the_request_number_encodes() {
        // sizeof(struct dm_ioctl) is baked into every request number, so a wrong
        // value here makes every ioctl fail with ENOTTY rather than misbehave subtly.
        assert_eq!(DM_IOCTL_SIZE, 312);
        assert_eq!(DM_TARGET_SPEC_SIZE, 40);
        assert_eq!((iowr(0) >> 16) & 0x3fff, u64::from(DM_IOCTL_SIZE_U32));
    }

    #[test]
    fn the_header_carries_the_interface_version_the_kernel_checks() {
        let buffer = header_only_buffer("plexos-usr");
        assert_eq!(u32::from_ne_bytes(buffer[0..4].try_into().unwrap()), 4);
    }

    #[test]
    fn the_name_lands_at_the_offset_the_abi_specifies() {
        let buffer = header_only_buffer("plexos-usr");
        let name: Vec<u8> = buffer[NAME_OFFSET..]
            .iter()
            .take_while(|b| **b != 0)
            .copied()
            .collect();
        assert_eq!(String::from_utf8(name).unwrap(), "plexos-usr");
    }

    #[test]
    fn data_start_points_past_the_header() {
        // The kernel reads the target spec from `data_start`. Pointing it anywhere
        // else yields EINVAL with nothing to say which field was wrong.
        let buffer = table_load_buffer("d", 100, "verity", "1 /dev/a /dev/b");
        assert_eq!(
            u32::from_ne_bytes(buffer[16..20].try_into().unwrap()),
            DM_IOCTL_SIZE_U32
        );
    }

    #[test]
    fn a_table_load_declares_exactly_one_target() {
        let buffer = table_load_buffer("d", 100, "verity", "params");
        assert_eq!(u32::from_ne_bytes(buffer[20..24].try_into().unwrap()), 1);
    }

    #[test]
    fn the_table_is_loaded_read_only() {
        // A verity device that accepts writes is a contradiction, and the failure
        // would appear on first write rather than here.
        let buffer = table_load_buffer("d", 100, "verity", "params");
        let flags = u32::from_ne_bytes(buffer[28..32].try_into().unwrap());
        assert_eq!(flags & DM_READONLY_FLAG, DM_READONLY_FLAG);
    }

    #[test]
    fn the_target_spec_records_the_device_length_in_sectors() {
        let sb = superblock();
        let buffer = table_load_buffer("d", sb.sectors(), "verity", "params");
        let length = u64::from_ne_bytes(
            buffer[DM_IOCTL_SIZE + 8..DM_IOCTL_SIZE + 16]
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            length, 3904,
            "488 blocks of 4096 bytes, in 512-byte sectors"
        );
    }

    #[test]
    fn the_target_type_is_verity_and_is_nul_terminated() {
        let buffer = table_load_buffer("d", 1, "verity", "params");
        let start = DM_IOCTL_SIZE + 24;
        let name: Vec<u8> = buffer[start..start + 16]
            .iter()
            .take_while(|b| **b != 0)
            .copied()
            .collect();
        assert_eq!(String::from_utf8(name).unwrap(), "verity");
    }

    #[test]
    fn the_parameter_string_follows_the_spec_and_is_nul_terminated() {
        let params = "1 /dev/sda2 /dev/sda3 4096 4096 488 1 sha256 abc def";
        let buffer = table_load_buffer("d", 1, "verity", params);
        let start = DM_IOCTL_SIZE + DM_TARGET_SPEC_SIZE;
        let read: Vec<u8> = buffer[start..]
            .iter()
            .take_while(|b| **b != 0)
            .copied()
            .collect();
        assert_eq!(String::from_utf8(read).unwrap(), params);
        assert_eq!(buffer[start + params.len()], 0, "params must be terminated");
    }

    #[test]
    fn next_spans_the_spec_and_its_parameters() {
        // The kernel walks the table using `next`. Too small and it parses the tail
        // of the parameter string as another target.
        let params = "some params";
        let buffer = table_load_buffer("d", 1, "verity", params);
        let next = u32::from_ne_bytes(
            buffer[DM_IOCTL_SIZE + 20..DM_IOCTL_SIZE + 24]
                .try_into()
                .unwrap(),
        ) as usize;
        assert_eq!(next, DM_TARGET_SPEC_SIZE + align8(params.len() + 1));
        assert_eq!(DM_IOCTL_SIZE + next, buffer.len());
    }

    #[test]
    fn data_size_covers_the_whole_buffer() {
        let buffer = table_load_buffer("d", 1, "verity", "p");
        assert_eq!(
            u32::from_ne_bytes(buffer[12..16].try_into().unwrap()) as usize,
            buffer.len()
        );
    }

    #[test]
    fn the_payload_stays_eight_byte_aligned() {
        for params in ["a", "ab", "abcdefg", "abcdefgh", "abcdefghi"] {
            let buffer = table_load_buffer("d", 1, "verity", params);
            assert_eq!(buffer.len() % 8, 0, "unaligned for {params:?}");
        }
    }

    #[test]
    fn an_over_long_device_name_is_refused_rather_than_truncated() {
        let sb = superblock();
        let name = "x".repeat(DM_NAME_LEN);
        let error = create_verity(&name, "/dev/a", "/dev/b", "hash", &sb).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("too long"), "{error}");
    }

    #[test]
    fn the_full_verity_table_is_what_the_kernel_documents() {
        let sb = superblock();
        let params = sb.table_line(
            "/dev/disk/by-partlabel/usr_a",
            "/dev/disk/by-partlabel/usr_a_hash",
            "deadbeef",
        );
        let buffer = table_load_buffer("plexos-usr", sb.sectors(), "verity", &params);
        let start = DM_IOCTL_SIZE + DM_TARGET_SPEC_SIZE;
        let read: Vec<u8> = buffer[start..]
            .iter()
            .take_while(|b| **b != 0)
            .copied()
            .collect();
        assert_eq!(
            String::from_utf8(read).unwrap(),
            "1 /dev/disk/by-partlabel/usr_a /dev/disk/by-partlabel/usr_a_hash \
             4096 4096 488 1 sha256 deadbeef 706c65786f7300000000000000000000"
        );
    }
}
