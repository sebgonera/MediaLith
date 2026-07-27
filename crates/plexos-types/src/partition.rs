//! The on-disk partition contract (ADR-0003).
//!
//! This module is the single definition of PlexOS's disk layout. Nothing else may
//! hardcode a type GUID or a partition label; the installer, the updater, and
//! `plexos-init` all read them from here, so a typo is impossible to introduce in one
//! place and not the others.
//!
//! Type GUIDs come from the Discoverable Partitions Specification, so that a PlexOS
//! disk is legible to `blkid`, `sfdisk`, and `systemd-repart` without PlexOS-specific
//! knowledge. Slot identity is carried in the partition *label*, because the two `/usr`
//! slots are interchangeable and must share a type.
//!
//! # Verification
//!
//! Every GUID below was checked byte-for-byte against systemd v258.7
//! `src/systemd/sd-gpt.h`, which is the reference implementation of the specification.
//! Two of the four were wrong when first written, and both were wrong in the way that
//! is hardest to notice: the values were syntactically valid, unique, and unassigned,
//! so every test in this module passed and `sfdisk` accepted them without complaint.
//! The disk was simply illegible to the standard tooling these GUIDs exist to satisfy.
//!
//! `guids_match_the_discoverable_partitions_specification` below now pins all four.
//! Re-run it, do not re-derive the values by hand, and never take them from memory —
//! a wrong GUID here cannot be corrected by an update, only by reinstalling every
//! device.

use std::fmt;

/// EFI System Partition.
pub const GUID_ESP: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

/// `/usr` partition, x86-64.
pub const GUID_USR_X86_64: &str = "8484680c-9521-48c6-9c11-b0720656f69e";

/// dm-verity hash partition for `/usr`, x86-64.
pub const GUID_USR_VERITY_X86_64: &str = "77ff5f63-e7b6-4633-acf4-1565b864c0e6";

/// `/var` partition.
pub const GUID_VAR: &str = "4d21b016-b534-45c2-a9fb-5c16e091fd2d";

/// Partition label of the ESP.
pub const LABEL_ESP: &str = "esp";
/// Partition label of the persistent partition.
pub const LABEL_VAR: &str = "var";

/// One of the two interchangeable `/usr` slots.
///
/// Exactly two exist. Three-slot schemes are foreclosed by the fixed partition layout
/// in ADR-0003; this enum makes that constraint explicit rather than implicit in the
/// number of partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// First slot.
    A,
    /// Second slot.
    B,
}

impl Slot {
    /// Every slot, in canonical order.
    pub const ALL: [Self; 2] = [Self::A, Self::B];

    /// The slot an update is written to while this one is running.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    /// Partition label of this slot's `/usr` image, e.g. `usr_a`.
    #[must_use]
    pub const fn usr_label(self) -> &'static str {
        match self {
            Self::A => "usr_a",
            Self::B => "usr_b",
        }
    }

    /// Partition label of this slot's verity hash partition, e.g. `usr_a_hash`.
    #[must_use]
    pub const fn verity_label(self) -> &'static str {
        match self {
            Self::A => "usr_a_hash",
            Self::B => "usr_b_hash",
        }
    }

    /// Parses a slot from a `/usr` partition label.
    #[must_use]
    pub fn from_usr_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.usr_label() == label)
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::A => "a",
            Self::B => "b",
        })
    }
}

/// A partition in the canonical layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionSpec {
    /// GPT partition label. Slot identity lives here, not in the type GUID.
    pub label: &'static str,
    /// Discoverable Partitions Specification type GUID.
    pub type_guid: &'static str,
    /// Fixed size in mebibytes, or `None` to consume the remainder of the disk.
    pub size_mib: Option<u64>,
}

/// The canonical x86-64 layout, in partition-number order.
///
/// Order is part of the contract: an installer that emits these in a different order
/// produces a disk that later releases cannot reason about.
pub const LAYOUT_X86_64: [PartitionSpec; 6] = [
    PartitionSpec {
        label: LABEL_ESP,
        type_guid: GUID_ESP,
        size_mib: Some(512),
    },
    PartitionSpec {
        label: "usr_a",
        type_guid: GUID_USR_X86_64,
        size_mib: Some(1024),
    },
    PartitionSpec {
        label: "usr_a_hash",
        type_guid: GUID_USR_VERITY_X86_64,
        size_mib: Some(32),
    },
    PartitionSpec {
        label: "usr_b",
        type_guid: GUID_USR_X86_64,
        size_mib: Some(1024),
    },
    PartitionSpec {
        label: "usr_b_hash",
        type_guid: GUID_USR_VERITY_X86_64,
        size_mib: Some(32),
    },
    PartitionSpec {
        label: LABEL_VAR,
        type_guid: GUID_VAR,
        size_mib: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn is_lowercase_guid(s: &str) -> bool {
        let groups = [8, 4, 4, 4, 12];
        let parts: Vec<&str> = s.split('-').collect();
        parts.len() == groups.len()
            && parts.iter().zip(groups).all(|(part, len)| {
                part.len() == len
                    && part
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
            })
    }

    /// The values below are transcribed from systemd v258.7
    /// `src/systemd/sd-gpt.h`, decoding each `SD_ID128_MAKE(...)` byte list into its
    /// canonical UUID form:
    ///
    /// ```text
    /// SD_GPT_ESP               c1,2a,73,28,f8,1f,11,d2,ba,4b,00,a0,c9,3e,c9,3b
    /// SD_GPT_USR_X86_64        84,84,68,0c,95,21,48,c6,9c,11,b0,72,06,56,f6,9e
    /// SD_GPT_USR_X86_64_VERITY 77,ff,5f,63,e7,b6,46,33,ac,f4,15,65,b8,64,c0,e6
    /// SD_GPT_VAR               4d,21,b0,16,b5,34,45,c2,a9,fb,5c,16,e0,91,fd,2d
    /// ```
    ///
    /// This is the one test in the crate whose expected values must never be updated
    /// to match the code. If it fails, the code is wrong.
    #[test]
    fn guids_match_the_discoverable_partitions_specification() {
        for (name, actual, expected) in [
            ("ESP", GUID_ESP, "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"),
            (
                "USR_X86_64",
                GUID_USR_X86_64,
                "8484680c-9521-48c6-9c11-b0720656f69e",
            ),
            (
                "USR_X86_64_VERITY",
                GUID_USR_VERITY_X86_64,
                "77ff5f63-e7b6-4633-acf4-1565b864c0e6",
            ),
            ("VAR", GUID_VAR, "4d21b016-b534-45c2-a9fb-5c16e091fd2d"),
        ] {
            assert_eq!(
                actual, expected,
                "{name} does not match the Discoverable Partitions Specification. \
                 Correct the constant; do not change this expectation."
            );
        }
    }

    #[test]
    fn guids_are_well_formed_and_lowercase() {
        for spec in LAYOUT_X86_64 {
            assert!(
                is_lowercase_guid(spec.type_guid),
                "{}: malformed GUID {}",
                spec.label,
                spec.type_guid
            );
        }
    }

    #[test]
    fn labels_are_unique() {
        let mut seen = Vec::new();
        for spec in LAYOUT_X86_64 {
            assert!(
                !seen.contains(&spec.label),
                "duplicate partition label {}",
                spec.label
            );
            seen.push(spec.label);
        }
    }

    #[test]
    fn slots_share_a_type_guid_and_differ_only_by_label() {
        let usr: Vec<&PartitionSpec> = LAYOUT_X86_64
            .iter()
            .filter(|s| s.type_guid == GUID_USR_X86_64)
            .collect();
        assert_eq!(usr.len(), 2, "there must be exactly two /usr slots");
        assert_ne!(usr[0].label, usr[1].label);
        assert_eq!(usr[0].size_mib, usr[1].size_mib, "slots must be equal size");
    }

    #[test]
    fn every_slot_has_a_matching_pair_of_partitions() {
        for slot in Slot::ALL {
            for label in [slot.usr_label(), slot.verity_label()] {
                assert!(
                    LAYOUT_X86_64.iter().any(|s| s.label == label),
                    "layout is missing {label}"
                );
            }
            assert_eq!(Slot::from_usr_label(slot.usr_label()), Some(slot));
            assert_ne!(slot.other(), slot);
        }
    }

    #[test]
    fn only_the_last_partition_is_growable() {
        let (last, rest) = LAYOUT_X86_64.split_last().unwrap();
        assert!(last.size_mib.is_none(), "/var must take the remainder");
        assert!(
            rest.iter().all(|s| s.size_mib.is_some()),
            "only the final partition may be unsized"
        );
    }

    #[test]
    fn esp_holds_three_unified_kernel_images() {
        // ADR-0003: an update stages a third UKI alongside the two installed ones.
        // A UKI with drivers built in is budgeted at 128 MiB.
        let esp = LAYOUT_X86_64[0].size_mib.unwrap();
        assert!(
            esp >= 3 * 128,
            "ESP too small to stage an update: {esp} MiB"
        );
    }
}
