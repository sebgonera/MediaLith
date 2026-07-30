//! Turning the frozen layout into the bytes of a GUID Partition Table (ADR-0016).
//!
//! [`crate::partition`] says what the partitions are. This says what they look like on a
//! disk: a protective MBR, a primary header and entry array near the front, and a second
//! copy of both at the back.
//!
//! # Why this is here rather than delegated to `sfdisk`
//!
//! The first plan was to add `sfdisk` to the image and feed it the script
//! [`crate::bin`]'s layout emitter already produces. It was the wrong plan for a reason
//! this repository has recorded three times: **a program in the image is not a program
//! that can do the job.** `erofs-utils` built without `lz4`, `busybox tar` without `xz`,
//! `busybox losetup` without `--show` — each present, each unable, each discovered minutes
//! into a long operation. Adding a fourth such dependency to the most destructive thing the
//! system does is a bet against our own history.
//!
//! What makes writing it acceptable is not confidence in this code. It is that the result
//! is checked by tools that are *not* this code: the tests write a table into a file and
//! hand it to `sgdisk --verify` and `sfdisk --list`. Same rule as pinning the verity digest
//! against `sha256sum`, and the same rule that caught two partition GUIDs which were
//! well-formed, unique, correctly paired, and not the ones the specification defines.
//!
//! # Everything here is pure
//!
//! No file is opened and no randomness is drawn. The identifiers that must be unique per
//! disk are arguments, because a function that invented them could not be tested for
//! producing the same disk twice — and because this crate does no I/O at all.

use crate::partition::{LAYOUT_X86_64, PartitionSpec};

/// Bytes in a GPT partition entry, fixed by the specification.
pub const ENTRY_SIZE: u64 = 128;

/// Entries in the array. 128 is what the specification requires support for and what
/// every tool expects; a smaller array is legal and surprises things.
pub const ENTRY_COUNT: u64 = 128;

/// Signature at the start of a GPT header.
const SIGNATURE: &[u8; 8] = b"EFI PART";

/// Revision 1.0, as a little-endian u32.
const REVISION: u32 = 0x0001_0000;

/// Bytes of the header that the CRC covers.
const HEADER_SIZE: u32 = 92;

/// Partitions start on a mebibyte boundary.
///
/// Not a specification requirement. It is what every other tool does, and the reason is
/// erase blocks: a partition that starts halfway through one makes every write to it a
/// read-modify-write on an SSD, for the life of the disk.
pub const ALIGNMENT_BYTES: u64 = 1024 * 1024;

/// The smallest `/var` worth installing onto.
///
/// `/var` takes what is left after the fixed partitions (ADR-0003), and what is left can be
/// nothing. Refusing here rather than producing a valid table with a 4 MiB `/var` is the
/// difference between "this disk is too small" and a machine that installs, boots, and
/// fills up during its first Plex library scan.
pub const MINIMUM_VAR_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// A disk, as much of it as this needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disk {
    /// Total addressable sectors.
    pub sectors: u64,
    /// Bytes per sector: 512 or 4096.
    pub sector_size: u64,
}

impl Disk {
    /// Capacity in bytes.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.sectors * self.sector_size
    }
}

/// The identifiers that make one installed disk distinguishable from another.
///
/// Arguments rather than something this module generates, because a disk GUID drawn from a
/// random source inside a pure function cannot be tested, and because this crate opens no
/// files. The installer draws them from `/dev/urandom`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The disk's own GUID.
    pub disk: [u8; 16],
    /// One GUID per partition, in layout order.
    pub partitions: Vec<[u8; 16]>,
}

/// Bytes to be written at a sector offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// Sector to write at.
    pub lba: u64,
    /// What to write there.
    pub bytes: Vec<u8>,
}

/// Why a table could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GptError {
    /// The sector size is not one this understands.
    SectorSize(u64),
    /// The disk cannot hold the layout.
    TooSmall {
        /// What the disk has, in bytes.
        capacity: u64,
        /// What the layout needs, in bytes.
        needed: u64,
    },
    /// The identity does not carry one GUID per partition.
    WrongIdentityLength {
        /// How many were given.
        given: usize,
        /// How many the layout has.
        wanted: usize,
    },
    /// A type GUID in the layout is not a GUID.
    BadGuid(&'static str),
}

impl std::fmt::Display for GptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SectorSize(size) => write!(
                f,
                "this disk reports {size}-byte sectors, and PlexOS understands 512 and \
                 4096. Remedy: none from here -- the layout's sizes are in mebibytes and \
                 would not divide cleanly, and guessing would produce a table that tools \
                 disagree about."
            ),
            Self::TooSmall { capacity, needed } => write!(
                f,
                "this disk holds {} GiB and PlexOS needs at least {} GiB. Remedy: use a \
                 larger disk. The fixed partitions come to about 2.6 GiB (ADR-0003) and \
                 the rest is /var, which has to be big enough for a media database and \
                 transcoding scratch.",
                capacity / (1024 * 1024 * 1024),
                needed / (1024 * 1024 * 1024)
            ),
            Self::WrongIdentityLength { given, wanted } => write!(
                f,
                "{given} partition identifiers were supplied for a layout with {wanted} \
                 partitions. Remedy: this is a programming error rather than anything \
                 about the disk."
            ),
            Self::BadGuid(guid) => write!(
                f,
                "{guid} is not a GUID. Remedy: a programming error in \
                 plexos_types::partition, which its own tests should have caught."
            ),
        }
    }
}

impl std::error::Error for GptError {}

/// Where each partition lands, once the arithmetic is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// The partition being placed.
    pub spec: PartitionSpec,
    /// First sector, inclusive.
    pub first_lba: u64,
    /// Last sector, inclusive.
    pub last_lba: u64,
}

impl Placement {
    /// Size in bytes, for reporting.
    #[must_use]
    pub const fn bytes(&self, sector_size: u64) -> u64 {
        (self.last_lba - self.first_lba + 1) * sector_size
    }
}

/// Works out where every partition goes, without producing any bytes.
///
/// Separated from [`table`] because it is the part worth reading in a report: a person
/// deciding whether to erase a disk is owed the sizes it would end up with, and computing
/// them twice in two places is how the report and the disk come to disagree.
///
/// # Errors
/// [`GptError::SectorSize`] or [`GptError::TooSmall`].
pub fn plan(disk: Disk) -> Result<Vec<Placement>, GptError> {
    if disk.sector_size != 512 && disk.sector_size != 4096 {
        return Err(GptError::SectorSize(disk.sector_size));
    }

    let array_sectors = (ENTRY_COUNT * ENTRY_SIZE).div_ceil(disk.sector_size);
    let alignment = ALIGNMENT_BYTES / disk.sector_size;

    // LBA 0 is the protective MBR, 1 the header, then the array. The backup is the same in
    // reverse at the end of the disk.
    let first_usable = 2 + array_sectors;
    let last_usable = disk
        .sectors
        .checked_sub(array_sectors + 2)
        .ok_or(GptError::TooSmall {
            capacity: disk.bytes(),
            needed: MINIMUM_VAR_BYTES,
        })?;

    let mut placements = Vec::new();
    let mut cursor = first_usable.div_ceil(alignment) * alignment;

    for spec in &LAYOUT_X86_64 {
        let first_lba = cursor;
        let last_lba = match spec.size_mib {
            Some(mib) => {
                let sectors = mib * ALIGNMENT_BYTES / disk.sector_size;
                let last = first_lba + sectors - 1;
                cursor = (last + 1).div_ceil(alignment) * alignment;
                last
            }
            // The remainder, rounded down so the partition ends where the usable area
            // does rather than one sector past it.
            None => last_usable,
        };

        if last_lba > last_usable || first_lba > last_usable {
            return Err(GptError::TooSmall {
                capacity: disk.bytes(),
                needed: fixed_bytes() + MINIMUM_VAR_BYTES,
            });
        }

        placements.push(Placement {
            spec: *spec,
            first_lba,
            last_lba,
        });
    }

    // Checked last, and separately from "does it fit". A disk that fits the fixed
    // partitions and leaves a gigabyte for /var produces a perfectly valid table and a
    // machine that fills up during its first library scan.
    let var = placements.last().ok_or(GptError::TooSmall {
        capacity: disk.bytes(),
        needed: fixed_bytes(),
    })?;
    if var.bytes(disk.sector_size) < MINIMUM_VAR_BYTES {
        return Err(GptError::TooSmall {
            capacity: disk.bytes(),
            needed: fixed_bytes() + MINIMUM_VAR_BYTES,
        });
    }

    Ok(placements)
}

/// Bytes taken by every partition that has a fixed size.
#[must_use]
pub fn fixed_bytes() -> u64 {
    LAYOUT_X86_64
        .iter()
        .filter_map(|spec| spec.size_mib)
        .map(|mib| mib * ALIGNMENT_BYTES)
        .sum()
}

/// The complete table, as regions to write.
///
/// Four regions: the protective MBR and primary header and array at the front, and the
/// backup array and header at the back. Nothing else on the disk is touched, which is
/// deliberate — the filesystems are made afterwards and by something else.
///
/// # Errors
/// As [`plan`], plus [`GptError::WrongIdentityLength`] and [`GptError::BadGuid`].
pub fn table(disk: Disk, identity: &Identity) -> Result<Vec<Region>, GptError> {
    let placements = plan(disk)?;

    if identity.partitions.len() != placements.len() {
        return Err(GptError::WrongIdentityLength {
            given: identity.partitions.len(),
            wanted: placements.len(),
        });
    }

    let array_sectors = (ENTRY_COUNT * ENTRY_SIZE).div_ceil(disk.sector_size);
    let primary_array_lba = 2;
    let backup_header_lba = disk.sectors - 1;
    let backup_array_lba = backup_header_lba - array_sectors;

    let mut entries = vec![0u8; usize::try_from(ENTRY_COUNT * ENTRY_SIZE).unwrap_or(0)];
    for (index, placement) in placements.iter().enumerate() {
        let at = index * usize::try_from(ENTRY_SIZE).unwrap_or(0);
        write_entry(
            &mut entries[at..at + usize::try_from(ENTRY_SIZE).unwrap_or(0)],
            placement,
            identity.partitions[index],
        )?;
    }
    let entries_crc = crc32(&entries);

    let first_usable = 2 + array_sectors;
    let last_usable = backup_array_lba - 1;

    let primary = header(
        &identity.disk,
        1,
        backup_header_lba,
        primary_array_lba,
        first_usable,
        last_usable,
        entries_crc,
    );
    let backup = header(
        &identity.disk,
        backup_header_lba,
        1,
        backup_array_lba,
        first_usable,
        last_usable,
        entries_crc,
    );

    Ok(vec![
        Region {
            lba: 0,
            bytes: protective_mbr(disk),
        },
        Region {
            lba: 1,
            bytes: pad(primary, disk.sector_size),
        },
        Region {
            lba: primary_array_lba,
            bytes: entries.clone(),
        },
        Region {
            lba: backup_array_lba,
            bytes: entries,
        },
        Region {
            lba: backup_header_lba,
            bytes: pad(backup, disk.sector_size),
        },
    ])
}

/// Pads a buffer out to a whole sector.
fn pad(mut bytes: Vec<u8>, sector_size: u64) -> Vec<u8> {
    bytes.resize(usize::try_from(sector_size).unwrap_or(512), 0);
    bytes
}

/// The 92-byte header, with its own CRC filled in.
fn header(
    disk_guid: &[u8; 16],
    my_lba: u64,
    alternate_lba: u64,
    array_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entries_crc: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; usize::try_from(HEADER_SIZE).unwrap_or(92)];
    out[0..8].copy_from_slice(SIGNATURE);
    out[8..12].copy_from_slice(&REVISION.to_le_bytes());
    out[12..16].copy_from_slice(&HEADER_SIZE.to_le_bytes());
    // 16..20 is the header CRC and stays zero while it is computed.
    out[24..32].copy_from_slice(&my_lba.to_le_bytes());
    out[32..40].copy_from_slice(&alternate_lba.to_le_bytes());
    out[40..48].copy_from_slice(&first_usable.to_le_bytes());
    out[48..56].copy_from_slice(&last_usable.to_le_bytes());
    out[56..72].copy_from_slice(disk_guid);
    out[72..80].copy_from_slice(&array_lba.to_le_bytes());
    out[80..84].copy_from_slice(&u32::try_from(ENTRY_COUNT).unwrap_or(128).to_le_bytes());
    out[84..88].copy_from_slice(&u32::try_from(ENTRY_SIZE).unwrap_or(128).to_le_bytes());
    out[88..92].copy_from_slice(&entries_crc.to_le_bytes());

    let crc = crc32(&out);
    out[16..20].copy_from_slice(&crc.to_le_bytes());
    out
}

/// One 128-byte partition entry.
fn write_entry(out: &mut [u8], placement: &Placement, unique: [u8; 16]) -> Result<(), GptError> {
    let type_guid =
        guid_bytes(placement.spec.type_guid).ok_or(GptError::BadGuid(placement.spec.type_guid))?;

    out[0..16].copy_from_slice(&type_guid);
    out[16..32].copy_from_slice(&unique);
    out[32..40].copy_from_slice(&placement.first_lba.to_le_bytes());
    out[40..48].copy_from_slice(&placement.last_lba.to_le_bytes());
    // Attributes: none. The bootloader finds its entries by type GUID and label, and the
    // legacy BIOS bootable bit means nothing on a UEFI machine.
    out[48..56].copy_from_slice(&0u64.to_le_bytes());

    // The name is UTF-16LE in 72 bytes, so 36 code units including no terminator. Labels
    // here are short ASCII by ADR-0003, and truncation is silent in the specification --
    // which is why the layout's own tests bound their length rather than trusting this.
    for (index, unit) in placement.spec.label.encode_utf16().take(36).enumerate() {
        let at = 56 + index * 2;
        out[at..at + 2].copy_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

/// The protective MBR at LBA 0.
///
/// One entry of type `0xEE` covering the whole disk, so a tool that understands only MBR
/// sees a disk that is entirely in use rather than an empty one it might offer to
/// partition.
fn protective_mbr(disk: Disk) -> Vec<u8> {
    let mut out = vec![0u8; usize::try_from(disk.sector_size).unwrap_or(512)];

    let at = 446;
    out[at] = 0x00; // not bootable
    out[at + 1] = 0x00; // CHS first: head
    out[at + 2] = 0x02; // sector 2
    out[at + 3] = 0x00; // cylinder
    out[at + 4] = 0xEE; // GPT protective
    out[at + 5] = 0xFF; // CHS last, saturated
    out[at + 6] = 0xFF;
    out[at + 7] = 0xFF;
    out[at + 8..at + 12].copy_from_slice(&1u32.to_le_bytes());

    // Saturated rather than wrapped: the field is 32 bits and a disk larger than 2 TiB
    // cannot be described in it. Every tool reads 0xFFFFFFFF as "all of it".
    let sectors = u32::try_from(disk.sectors - 1).unwrap_or(u32::MAX);
    out[at + 12..at + 16].copy_from_slice(&sectors.to_le_bytes());

    out[510] = 0x55;
    out[511] = 0xAA;
    out
}

/// Parses `c12a7328-f81f-11d2-ba4b-00a0c93ec93b` into the bytes a GPT stores.
///
/// The first three groups are little-endian and the last two are not. That mixed encoding
/// is the single most common way to write a partition table that every tool reads as
/// having the wrong type — the bytes are all present and three of the fields are backwards.
#[must_use]
pub fn guid_bytes(text: &str) -> Option<[u8; 16]> {
    let groups: Vec<&str> = text.split('-').collect();
    if groups.len() != 5
        || [8, 4, 4, 4, 12] != groups.iter().map(|g| g.len()).collect::<Vec<_>>()[..]
    {
        return None;
    }

    let hex = |group: &str| -> Option<Vec<u8>> {
        (0..group.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&group[i..i + 2], 16).ok())
            .collect()
    };

    let mut out = [0u8; 16];
    let mut at = 0;
    for (index, group) in groups.iter().enumerate() {
        let mut bytes = hex(group)?;
        if index < 3 {
            bytes.reverse();
        }
        out[at..at + bytes.len()].copy_from_slice(&bytes);
        at += bytes.len();
    }
    Some(out)
}

/// CRC-32/ISO-HDLC, the one GPT uses.
///
/// Written out rather than taken from a crate: it is fifteen lines, it has a published
/// test vector, and the alternative is a dependency in the crate every other one depends
/// on.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disk the size of the reference laptop's internal one.
    const KINGSTON: Disk = Disk {
        sectors: 976_773_168,
        sector_size: 512,
    };

    #[test]
    fn the_crc_matches_the_published_check_value() {
        // CRC-32/ISO-HDLC's check value: the one every catalogue lists for "123456789".
        // Pinned against a published number rather than against this function's own
        // output, which is the difference between a test and a tautology.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn a_guid_is_stored_with_its_first_three_groups_reversed() {
        // The single commonest way to produce a table every tool reads as having the wrong
        // partition types: all sixteen bytes present, three fields backwards. Pinned
        // against the EFI system partition GUID as the specification writes it.
        let bytes = guid_bytes("c12a7328-f81f-11d2-ba4b-00a0c93ec93b").expect("a GUID");
        assert_eq!(
            bytes,
            [
                0x28, 0x73, 0x2a, 0xc1, // c12a7328, reversed
                0x1f, 0xf8, // f81f, reversed
                0xd2, 0x11, // 11d2, reversed
                0xba, 0x4b, // ba4b, as written
                0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b, // and the rest
            ]
        );
    }

    #[test]
    fn something_that_is_not_a_guid_is_refused_rather_than_padded() {
        for bad in [
            "",
            "not-a-guid",
            "c12a7328f81f11d2ba4b00a0c93ec93b",
            "zz2a7328-f81f-11d2-ba4b-00a0c93ec93b",
        ] {
            assert_eq!(guid_bytes(bad), None, "{bad} was accepted");
        }
    }

    #[test]
    fn every_partition_is_aligned_and_none_overlap() {
        // Misalignment costs nothing visible and every write to the disk for its whole
        // life; overlap costs the data in whichever partition is written second.
        let placements = plan(KINGSTON).expect("a 465 GiB disk fits");
        assert_eq!(placements.len(), LAYOUT_X86_64.len());

        let alignment = ALIGNMENT_BYTES / KINGSTON.sector_size;
        let mut previous_end = 0;
        for placement in &placements {
            assert_eq!(
                placement.first_lba % alignment,
                0,
                "{} starts at {}",
                placement.spec.label,
                placement.first_lba
            );
            assert!(
                placement.first_lba > previous_end,
                "{} overlaps what came before",
                placement.spec.label
            );
            assert!(placement.last_lba >= placement.first_lba);
            previous_end = placement.last_lba;
        }
    }

    #[test]
    fn the_sizes_are_the_ones_adr_0003_froze() {
        let placements = plan(KINGSTON).unwrap();
        let mib = |p: &Placement| p.bytes(KINGSTON.sector_size) / (1024 * 1024);

        assert_eq!(mib(&placements[0]), 512, "esp");
        assert_eq!(mib(&placements[1]), 1024, "usr_a");
        assert_eq!(mib(&placements[2]), 32, "usr_a_hash");
        assert_eq!(mib(&placements[3]), 1024, "usr_b");
        assert_eq!(mib(&placements[4]), 32, "usr_b_hash");
        assert!(
            mib(&placements[5]) > 400 * 1024,
            "var takes the remainder of a 465 GiB disk, got {} MiB",
            mib(&placements[5])
        );
    }

    #[test]
    fn a_disk_too_small_for_a_usable_var_is_refused_with_both_numbers() {
        // It would produce a perfectly valid table. The machine would install, boot, and
        // fill up during its first library scan, which is a fault nobody would connect to
        // the installer.
        let small = Disk {
            sectors: 6 * 1024 * 1024 * 1024 / 512,
            sector_size: 512,
        };
        let error = plan(small).unwrap_err();
        assert!(matches!(error, GptError::TooSmall { .. }));
        let message = error.to_string();
        assert!(message.contains("Remedy:"), "{message}");
        assert!(message.contains("larger disk"), "{message}");
    }

    #[test]
    fn an_unfamiliar_sector_size_is_refused_rather_than_guessed_at() {
        let odd = Disk {
            sectors: 1_000_000,
            sector_size: 520,
        };
        assert_eq!(plan(odd).unwrap_err(), GptError::SectorSize(520));
        assert!(
            plan(Disk {
                sector_size: 4096,
                ..KINGSTON
            })
            .is_ok()
        );
    }

    fn identity() -> Identity {
        Identity {
            disk: [0x11; 16],
            partitions: (0..LAYOUT_X86_64.len())
                .map(|i| {
                    let mut guid = [0x22; 16];
                    guid[15] = u8::try_from(i).unwrap_or(0);
                    guid
                })
                .collect(),
        }
    }

    #[test]
    fn the_table_lands_where_the_specification_says() {
        let regions = table(KINGSTON, &identity()).expect("a table");
        assert_eq!(regions.len(), 5);

        assert_eq!(regions[0].lba, 0, "protective MBR");
        assert_eq!(regions[1].lba, 1, "primary header");
        assert_eq!(regions[2].lba, 2, "primary entry array");
        assert_eq!(
            regions[4].lba,
            KINGSTON.sectors - 1,
            "the backup header is the last sector, and a disk whose end is wrong is one \
             every tool reports as corrupt"
        );
        assert_eq!(regions[3].lba, KINGSTON.sectors - 1 - 32);

        assert_eq!(&regions[1].bytes[0..8], SIGNATURE);
        assert_eq!(&regions[4].bytes[0..8], SIGNATURE);
        assert_eq!(regions[0].bytes[510], 0x55);
        assert_eq!(regions[0].bytes[511], 0xAA);
        assert_eq!(regions[0].bytes[450], 0xEE, "protective type");
    }

    #[test]
    fn the_two_headers_agree_about_everything_but_where_they_are() {
        // A backup that describes a different disk is what a tool reports when the primary
        // is damaged, and following it would move every partition.
        let regions = table(KINGSTON, &identity()).unwrap();
        let (primary, backup) = (&regions[1].bytes, &regions[4].bytes);

        assert_eq!(primary[40..56], backup[40..56], "usable area");
        assert_eq!(primary[56..72], backup[56..72], "disk GUID");
        assert_eq!(primary[88..92], backup[88..92], "entry array CRC");
        assert_ne!(primary[24..32], backup[24..32], "each says where it is");
        assert_eq!(primary[24..32], backup[32..40], "and where the other is");
    }

    #[test]
    fn an_identity_that_does_not_match_the_layout_is_refused() {
        let mut wrong = identity();
        wrong.partitions.pop();
        assert!(matches!(
            table(KINGSTON, &wrong),
            Err(GptError::WrongIdentityLength {
                given: 5,
                wanted: 6
            })
        ));
    }

    #[test]
    fn labels_are_readable_back_out_of_the_entries() {
        let regions = table(KINGSTON, &identity()).unwrap();
        let entries = &regions[2].bytes;

        for (index, spec) in LAYOUT_X86_64.iter().enumerate() {
            let at = index * 128 + 56;
            let units: Vec<u16> = (0..spec.label.len())
                .map(|i| u16::from_le_bytes([entries[at + i * 2], entries[at + i * 2 + 1]]))
                .collect();
            assert_eq!(String::from_utf16_lossy(&units), spec.label);
        }
    }

    /// Writes a table into a sparse file and hands it to a tool that is not this code.
    ///
    /// `None` when the tool is absent, announced rather than passed quietly: a check
    /// nobody knows was skipped is a check nobody has.
    fn inspect(name: &str, disk: Disk, program: &str, args: &[&str]) -> Option<String> {
        use std::io::{Seek as _, SeekFrom, Write as _};

        if std::process::Command::new(program)
            .arg("--version")
            .output()
            .is_err()
        {
            println!("skip: no {program} on this host, so the table was not verified");
            return None;
        }

        let path = std::env::temp_dir().join(format!("plexos-gpt-{name}.img"));
        let mut file = std::fs::File::create(&path).ok()?;
        // Sparse: the file is the size of the disk and occupies almost nothing, which is
        // how a 465 GiB disk can be tested on a laptop.
        file.set_len(disk.bytes()).ok()?;

        for region in table(disk, &identity()).ok()? {
            file.seek(SeekFrom::Start(region.lba * disk.sector_size))
                .ok()?;
            file.write_all(&region.bytes).ok()?;
        }
        file.sync_all().ok()?;
        drop(file);

        let out = std::process::Command::new(program)
            .args(args)
            .arg(&path)
            .output()
            .ok()?;
        let _ = std::fs::remove_file(&path);

        Some(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }

    #[test]
    fn sgdisk_finds_nothing_wrong_with_what_this_produces() {
        // The whole justification for writing a partition table by hand instead of adding
        // a tool to the image. Every check below is somebody else's opinion of these
        // bytes: the CRCs, the backup header's position, the usable range, the alignment.
        let Some(report) = inspect("verify", KINGSTON, "sgdisk", &["--verify"]) else {
            return;
        };

        assert!(
            report.contains("No problems found"),
            "sgdisk objected to this table:\n{report}"
        );
    }

    #[test]
    fn sgdisk_reads_back_the_layout_adr_0003_defines() {
        let Some(report) = inspect("print", KINGSTON, "sgdisk", &["--print"]) else {
            return;
        };

        for label in LAYOUT_X86_64.iter().map(|spec| spec.label) {
            assert!(report.contains(label), "{label} is missing from:\n{report}");
        }

        // The type codes sgdisk derives from the GUIDs, which is the assertion that
        // catches an endianness mistake: sixteen bytes that parse but mean something else
        // produce a different code, or none.
        for code in ["EF00", "8314", "8319", "8310"] {
            assert!(
                report.contains(code),
                "no partition of type {code}:\n{report}"
            );
        }

        // And the alignment, in its opinion rather than ours.
        assert!(
            report.contains("aligned on 2048-sector boundaries"),
            "{report}"
        );
    }

    #[test]
    fn the_esp_guid_reads_back_exactly_as_the_specification_writes_it() {
        // The narrowest possible check on the mixed-endian encoding, against a tool that
        // renders the GUID back as text. A byte order mistake here produces a table that
        // looks complete and that firmware will not boot from.
        let Some(report) = inspect("guid", KINGSTON, "sgdisk", &["-i", "1"]) else {
            return;
        };
        assert!(
            report.contains("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
            "{report}"
        );
        assert!(report.contains("EFI system partition"), "{report}");
    }

    #[test]
    fn sfdisk_agrees_with_sgdisk_about_the_same_bytes() {
        // Two implementations rather than one. They disagree about very little, and where
        // they do it is usually because the table is wrong in a way that one of them
        // tolerates -- which is exactly the class of defect that reaches a real disk.
        let Some(report) = inspect("sfdisk", KINGSTON, "sfdisk", &["--list"]) else {
            return;
        };

        assert!(
            report.contains("Disklabel type: gpt"),
            "not read as GPT:\n{report}"
        );
        assert!(
            !report.to_lowercase().contains("corrupt"),
            "sfdisk called it corrupt:\n{report}"
        );

        // The names come from the Discoverable Partitions Specification, so this is not
        // "sixteen bytes survived" -- it is a second implementation agreeing about what
        // those bytes *mean*, which is the whole reason ADR-0003 picked these GUIDs.
        for meaning in [
            "EFI System",
            "Linux /usr (x86-64)",
            "Linux /usr verity (x86-64)",
            "Linux variable data",
        ] {
            assert!(report.contains(meaning), "no {meaning} in:\n{report}");
        }
    }
}
