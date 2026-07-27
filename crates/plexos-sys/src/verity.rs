//! Reading the dm-verity superblock.
//!
//! To open a verity device the kernel needs more than the root hash: it needs the
//! block sizes, the number of data blocks, the hash algorithm, and the salt. All of
//! those were chosen by `veritysetup format` at image build time and recorded in a
//! 512-byte superblock at the start of the hash device, which is exactly why
//! `veritysetup open` needs only a root hash on the command line.
//!
//! `plexos-init` reads the same superblock for the same reason. The alternative —
//! putting block sizes and salt on the kernel command line — would mean the UKI and
//! the hash partition could disagree, and a mismatch there produces an I/O error at
//! boot rather than anything that names the cause.
//!
//! Nothing here is unsafe. It is byte parsing, and it lives in this crate only
//! because it is the natural companion to [`crate::dm`], which is not.
//!
//! The layout is fixed by the on-disk format that `cryptsetup` writes and the kernel
//! reads, so it cannot be changed by us:
//!
//! ```text
//!   0   signature[8]        "verity\0\0"
//!   8   version             u32   (1)
//!  12   hash_type           u32   (1 for the normal tree)
//!  16   uuid[16]
//!  32   algorithm[32]       NUL-padded, e.g. "sha256"
//!  64   data_block_size     u32
//!  68   hash_block_size     u32
//!  72   data_blocks         u64
//!  80   salt_size           u16
//!  82   padding[6]
//!  88   salt[256]
//! ```

use std::fmt;

/// Size of the superblock on disk.
pub const SUPERBLOCK_BYTES: usize = 512;

/// The magic at the start of a verity hash device.
const SIGNATURE: &[u8; 8] = b"verity\0\0";

/// The only superblock version this code understands.
const SUPPORTED_VERSION: u32 = 1;

/// Largest salt the format can carry.
const MAX_SALT: usize = 256;

/// Why a superblock could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerityError {
    /// The buffer was shorter than a superblock.
    TooShort {
        /// How many bytes were supplied.
        got: usize,
    },
    /// The signature did not match, so this is not a verity hash device.
    BadSignature,
    /// The superblock version is not one this code understands.
    UnsupportedVersion {
        /// Version found on disk.
        found: u32,
    },
    /// A field held a value that cannot describe a usable device.
    Invalid {
        /// Which field.
        field: &'static str,
        /// What it said.
        value: u64,
    },
}

impl fmt::Display for VerityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { got } => write!(
                f,
                "verity superblock is {got} bytes, need {SUPERBLOCK_BYTES}; \
                 check the hash partition is the one named on the command line"
            ),
            Self::BadSignature => write!(
                f,
                "no verity signature on the hash device; \
                 it was never formatted, or the wrong partition is being read"
            ),
            Self::UnsupportedVersion { found } => write!(
                f,
                "verity superblock version {found} is not supported (expected \
                 {SUPPORTED_VERSION}); the image was built by newer tooling than this init"
            ),
            Self::Invalid { field, value } => write!(
                f,
                "verity superblock field {field} is {value}, which cannot describe a \
                 usable device; the hash partition is corrupt and the slot should be \
                 failed rather than mounted"
            ),
        }
    }
}

impl std::error::Error for VerityError {}

/// The parameters needed to build a device-mapper verity table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VeritySuperblock {
    /// Bytes per data block.
    pub data_block_size: u32,
    /// Bytes per hash block.
    pub hash_block_size: u32,
    /// Number of data blocks the tree covers.
    pub data_blocks: u64,
    /// Hash algorithm name, e.g. `sha256`.
    pub algorithm: String,
    /// Salt, as lowercase hex. Empty when the salt length is zero.
    pub salt_hex: String,
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(buf)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl VeritySuperblock {
    /// Parses a superblock from the first [`SUPERBLOCK_BYTES`] of a hash device.
    ///
    /// # Errors
    ///
    /// Returns [`VerityError`] if the buffer is too short, is not a verity
    /// superblock, or describes a device that cannot be opened. Every one of those is
    /// a reason to fail the boot rather than continue: ADR-0004 makes a verity failure
    /// fatal by design, because falling back to an unverified mount would defeat the
    /// whole trust chain.
    pub fn parse(bytes: &[u8]) -> Result<Self, VerityError> {
        if bytes.len() < SUPERBLOCK_BYTES {
            return Err(VerityError::TooShort { got: bytes.len() });
        }
        if &bytes[0..8] != SIGNATURE {
            return Err(VerityError::BadSignature);
        }

        let version = u32_at(bytes, 8);
        if version != SUPPORTED_VERSION {
            return Err(VerityError::UnsupportedVersion { found: version });
        }

        let data_block_size = u32_at(bytes, 64);
        let hash_block_size = u32_at(bytes, 68);
        let data_blocks = u64_at(bytes, 72);
        let salt_size = u16_at(bytes, 80) as usize;

        // A zero block size would make the table nonsense and the device unopenable;
        // a non-power-of-two is rejected by the kernel anyway, and catching it here
        // produces a message that says which field was wrong.
        for (field, value) in [
            ("data_block_size", data_block_size),
            ("hash_block_size", hash_block_size),
        ] {
            if value == 0 || !value.is_power_of_two() {
                return Err(VerityError::Invalid {
                    field,
                    value: u64::from(value),
                });
            }
        }
        if data_blocks == 0 {
            return Err(VerityError::Invalid {
                field: "data_blocks",
                value: 0,
            });
        }
        if salt_size > MAX_SALT {
            return Err(VerityError::Invalid {
                field: "salt_size",
                value: salt_size as u64,
            });
        }

        let algorithm = bytes[32..64]
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as char)
            .collect::<String>();
        if algorithm.is_empty() {
            return Err(VerityError::Invalid {
                field: "algorithm",
                value: 0,
            });
        }

        Ok(Self {
            data_block_size,
            hash_block_size,
            data_blocks,
            algorithm,
            salt_hex: to_hex(&bytes[88..88 + salt_size]),
        })
    }

    /// The device-mapper target table line for this device.
    ///
    /// The field order is the kernel's, documented in
    /// `Documentation/admin-guide/device-mapper/verity.rst`, and is not ours to
    /// choose. `hash_start_block` is 1 because the superblock occupies hash block 0,
    /// which is what `veritysetup format` produces by default.
    ///
    /// A salt of zero length is written as `-`; an empty field would shift every
    /// later argument by one and the kernel would reject the table with a message
    /// that does not mention the salt.
    #[must_use]
    pub fn table_line(&self, data_device: &str, hash_device: &str, root_hash: &str) -> String {
        let salt = if self.salt_hex.is_empty() {
            "-"
        } else {
            &self.salt_hex
        };
        format!(
            "1 {data_device} {hash_device} {} {} {} 1 {} {root_hash} {salt}",
            self.data_block_size, self.hash_block_size, self.data_blocks, self.algorithm
        )
    }

    /// Size of the mapped device in 512-byte sectors, which the table needs.
    #[must_use]
    pub fn sectors(&self) -> u64 {
        self.data_blocks * u64::from(self.data_block_size) / 512
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A superblock written by `veritysetup format`, not one constructed here. A
    /// hand-built fixture would only prove this parser agrees with itself.
    const REAL: &[u8] = include_bytes!("../tests/fixtures/verity-superblock-sha256.bin");

    fn parsed() -> VeritySuperblock {
        VeritySuperblock::parse(REAL).expect("the fixture is a real superblock")
    }

    #[test]
    fn parses_what_veritysetup_wrote() {
        let sb = parsed();
        // Cross-checked against the `veritysetup format` output that produced this
        // fixture: 488 data blocks, 4096-byte blocks, sha256, salt 706c65786f7300...
        assert_eq!(sb.data_block_size, 4096);
        assert_eq!(sb.hash_block_size, 4096);
        assert_eq!(sb.data_blocks, 488);
        assert_eq!(sb.algorithm, "sha256");
        assert_eq!(sb.salt_hex, "706c65786f7300000000000000000000");
    }

    #[test]
    fn the_algorithm_string_stops_at_its_padding() {
        // The field is 32 NUL-padded bytes. Taking all of them would produce
        // "sha256\0\0\0..." and the kernel would reject the table.
        let sb = parsed();
        assert!(!sb.algorithm.contains('\0'), "{:?}", sb.algorithm);
        assert_eq!(sb.algorithm.len(), 6);
    }

    #[test]
    fn the_salt_is_read_at_its_declared_length() {
        // salt_size is 16 here but the field is 256 bytes wide. Reading the whole
        // field would append 240 bytes of zeros to the salt, and the resulting table
        // would compute different hashes and fail every block.
        let sb = parsed();
        assert_eq!(sb.salt_hex.len(), 32, "16 bytes as hex");
    }

    #[test]
    fn a_table_line_has_the_field_order_the_kernel_expects() {
        let sb = parsed();
        let line = sb.table_line("/dev/sda2", "/dev/sda3", "abc123");
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields[0], "1", "version");
        assert_eq!(fields[1], "/dev/sda2");
        assert_eq!(fields[2], "/dev/sda3");
        assert_eq!(fields[3], "4096", "data block size");
        assert_eq!(fields[4], "4096", "hash block size");
        assert_eq!(fields[5], "488", "data blocks");
        assert_eq!(fields[6], "1", "hash start block, past the superblock");
        assert_eq!(fields[7], "sha256");
        assert_eq!(fields[8], "abc123", "root hash");
        assert_eq!(fields[9], "706c65786f7300000000000000000000");
        assert_eq!(fields.len(), 10);
    }

    #[test]
    fn an_absent_salt_is_written_as_a_dash() {
        let mut sb = parsed();
        sb.salt_hex = String::new();
        let line = sb.table_line("/dev/a", "/dev/b", "hash");
        assert!(line.ends_with(" -"), "{line}");
        assert_eq!(line.split(' ').count(), 10, "field count must not change");
    }

    #[test]
    fn device_size_is_in_sectors_not_blocks() {
        // 488 blocks of 4096 bytes is 3904 sectors of 512. Passing blocks here would
        // map an eighth of the image and fail on the first read past it.
        assert_eq!(parsed().sectors(), 488 * 4096 / 512);
    }

    #[test]
    fn a_short_buffer_is_rejected() {
        assert_eq!(
            VeritySuperblock::parse(&REAL[..100]),
            Err(VerityError::TooShort { got: 100 })
        );
    }

    #[test]
    fn a_non_verity_device_is_rejected() {
        let zeros = [0u8; SUPERBLOCK_BYTES];
        assert_eq!(
            VeritySuperblock::parse(&zeros),
            Err(VerityError::BadSignature)
        );
    }

    #[test]
    fn a_future_superblock_version_is_refused_rather_than_guessed() {
        let mut bytes = REAL.to_vec();
        bytes[8] = 2;
        assert_eq!(
            VeritySuperblock::parse(&bytes),
            Err(VerityError::UnsupportedVersion { found: 2 })
        );
    }

    #[test]
    fn nonsense_block_sizes_are_rejected() {
        for (offset, field) in [(64, "data_block_size"), (68, "hash_block_size")] {
            let mut bytes = REAL.to_vec();
            bytes[offset..offset + 4].copy_from_slice(&0u32.to_le_bytes());
            assert_eq!(
                VeritySuperblock::parse(&bytes),
                Err(VerityError::Invalid { field, value: 0 })
            );

            // 4095 is not a power of two; the kernel would reject the table.
            let mut bytes = REAL.to_vec();
            bytes[offset..offset + 4].copy_from_slice(&4095u32.to_le_bytes());
            assert_eq!(
                VeritySuperblock::parse(&bytes),
                Err(VerityError::Invalid { field, value: 4095 })
            );
        }
    }

    #[test]
    fn an_empty_image_is_rejected() {
        let mut bytes = REAL.to_vec();
        bytes[72..80].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            VeritySuperblock::parse(&bytes),
            Err(VerityError::Invalid {
                field: "data_blocks",
                value: 0
            })
        );
    }

    #[test]
    fn every_error_names_a_remedy_or_a_consequence() {
        // The plexos-gpu rule, applied here: a diagnostic that stops at "verity
        // failed" has reproduced the problem this project exists to fix.
        let errors = [
            VerityError::TooShort { got: 1 },
            VerityError::BadSignature,
            VerityError::UnsupportedVersion { found: 9 },
            VerityError::Invalid {
                field: "data_blocks",
                value: 0,
            },
        ];
        for error in errors {
            let text = error.to_string();
            assert!(text.len() > 40, "too terse to act on: {text}");
            assert!(
                text.contains("check") || text.contains("should") || text.contains("was"),
                "no remedy or consequence in: {text}"
            );
        }
    }
}
