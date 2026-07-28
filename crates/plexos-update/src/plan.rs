//! Deciding whether to install a bundle, and into which slot.
//!
//! Pure, and separated from the writing for the usual reason plus one specific to this
//! crate: the decision is the part that can be wrong in a way nobody notices. Writing to
//! the wrong slot overwrites the system that is currently running, which is the one
//! outcome ADR-0001's two-slot layout exists to make impossible.

use plexos_types::{Slot, partition};

/// What to do about a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Install it into the named slot.
    Install {
        /// The slot to write, which is never the running one.
        target: Slot,
        /// The version being installed, for reporting.
        version: String,
    },
    /// The appliance already runs this version or a newer one.
    UpToDate {
        /// What is running.
        running: String,
    },
}

/// Why a bundle will not be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The image will not fit the slot it would go into.
    ImageTooLarge {
        /// Size the bundle declares.
        size: u64,
        /// Size the partition has.
        capacity: u64,
    },
    /// The verity tree will not fit its partition.
    VerityTooLarge {
        /// Size the bundle declares.
        size: u64,
        /// Size the partition has.
        capacity: u64,
    },
    /// The bundle's version does not sort above the running one, so the bootloader
    /// would keep choosing the entry already there.
    NotNewer {
        /// What the bundle offers.
        offered: String,
        /// What is running.
        running: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImageTooLarge { size, capacity } => write!(
                f,
                "the /usr image is {size} bytes and the slot holds {capacity}. The \
                 partition layout is frozen by ADR-0003 and cannot be grown in the \
                 field, so this is a bundle to rebuild smaller rather than a machine to \
                 change."
            ),
            Self::VerityTooLarge { size, capacity } => write!(
                f,
                "the verity hash tree is {size} bytes and its partition holds \
                 {capacity}. Same cause and same remedy as an oversized image: the \
                 layout is frozen."
            ),
            Self::NotNewer { offered, running } => write!(
                f,
                "the bundle offers {offered} and this appliance runs {running}, which \
                 does not sort below it. systemd-boot orders entries newest-first, so \
                 installing this would write a slot the bootloader then declines to \
                 choose -- an update that appears to do nothing. Publish a higher \
                 version."
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Size of a partition in the frozen layout, in bytes.
///
/// Looked up rather than written down twice: ADR-0003 owns these numbers and this must
/// not be the second place they live.
#[must_use]
pub fn capacity_of(label: &str) -> Option<u64> {
    partition::LAYOUT_X86_64
        .iter()
        .find(|spec| spec.label == label)
        .and_then(|spec| spec.size_mib)
        .map(|mib| mib * 1024 * 1024)
}

/// Decides what to do with a bundle on a machine running `running_slot` at
/// `running_version`.
///
/// # Errors
/// [`Refusal`] when the bundle cannot be installed at all. That is distinct from
/// [`Decision::UpToDate`], which is a normal answer to a normal question.
pub fn plan(
    running_slot: Slot,
    running_version: &str,
    bundle: &crate::Metadata,
) -> Result<Decision, Refusal> {
    // Never the running slot. This is the single most important line in the crate: the
    // running system's /usr is mounted read-only through dm-verity, and overwriting the
    // partition underneath it corrupts the machine that is doing the writing.
    let target = running_slot.other();

    let usr_capacity = capacity_of(target.usr_label()).unwrap_or(0);
    if bundle.usr.size > usr_capacity {
        return Err(Refusal::ImageTooLarge {
            size: bundle.usr.size,
            capacity: usr_capacity,
        });
    }

    let verity_capacity = capacity_of(target.verity_label()).unwrap_or(0);
    if bundle.verity.size > verity_capacity {
        return Err(Refusal::VerityTooLarge {
            size: bundle.verity.size,
            capacity: verity_capacity,
        });
    }

    match compare_versions(&bundle.version, running_version) {
        std::cmp::Ordering::Greater => Ok(Decision::Install {
            target,
            version: bundle.version.clone(),
        }),
        std::cmp::Ordering::Equal => Ok(Decision::UpToDate {
            running: running_version.to_owned(),
        }),
        std::cmp::Ordering::Less => Err(Refusal::NotNewer {
            offered: bundle.version.clone(),
            running: running_version.to_owned(),
        }),
    }
}

/// Orders two version strings the way `systemd-boot` orders boot entries.
///
/// Not a general-purpose implementation of `strverscmp_improved`, and it says so: it
/// handles dot-separated runs of digits, which is the whole of the format this project
/// publishes. Anything non-numeric compares bytewise, and a version with more components
/// sorts above a prefix of itself — so `0.1.0.202607281730` is newer than `0.1.0`, which
/// is exactly how a build stamp is meant to behave.
///
/// The reason this matters is not tidiness. If the appliance and the bootloader disagree
/// about which of two versions is newer, the updater writes a slot that the bootloader
/// then declines to boot, and the update silently does nothing.
#[must_use]
pub fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let mut mine = left.split('.');
    let mut theirs = right.split('.');
    loop {
        match (mine.next(), theirs.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(ours), Some(other)) => {
                let ordering = match (ours.parse::<u64>(), other.parse::<u64>()) {
                    (Ok(a), Ok(b)) => a.cmp(&b),
                    _ => ours.cmp(other),
                };
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    const HASH: &str = "b024b422b89fe9c8bd140915b3633c0819c183f83b45fc26b884d1d4971d2aa7";

    fn bundle(version: &str, usr_size: u64, verity_size: u64) -> crate::Metadata {
        let artifact = |name: &str, size: u64| crate::Artifact {
            name: name.to_owned(),
            size,
            sha256: HASH.to_owned(),
        };
        crate::Metadata {
            bundle_version: 1,
            version: version.to_owned(),
            root_hash: HASH.to_owned(),
            usr: artifact("usr.erofs", usr_size),
            verity: artifact("usr.hash", verity_size),
            uki_a: artifact("plexos-a.efi", 18_973_184),
            uki_b: artifact("plexos-b.efi", 18_973_184),
        }
    }

    fn ok() -> crate::Metadata {
        bundle("0.1.0.2", 74_448_896, 1_179_648)
    }

    #[test]
    fn an_update_never_writes_the_slot_it_is_running_from() {
        // The single most important property here. /usr is mounted read-only through
        // dm-verity from the running slot; writing that partition corrupts the machine
        // doing the writing, and the two-slot layout exists so that cannot happen.
        for running in Slot::ALL {
            let Decision::Install { target, .. } = plan(running, "0.1.0.1", &ok()).unwrap() else {
                panic!("a newer bundle installs");
            };
            assert_ne!(target, running);
            assert_eq!(target, running.other());
        }
    }

    #[test]
    fn the_same_version_is_up_to_date_rather_than_an_error() {
        // Asking "is there an update" and being told "no" is a normal answer to a normal
        // question, not a failure to report.
        let decision = plan(Slot::A, "0.1.0.2", &ok()).unwrap();
        assert_eq!(
            decision,
            Decision::UpToDate {
                running: "0.1.0.2".to_owned()
            }
        );
    }

    #[test]
    fn an_older_bundle_is_refused_because_the_bootloader_would_ignore_it() {
        // The failure this prevents is the confusing one: the write succeeds, the entry
        // is installed, and the machine reboots into the same version it had, because
        // systemd-boot orders entries newest-first and the old one still wins.
        let error = plan(Slot::A, "0.1.0.9", &ok()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("appears to do nothing"), "{message}");
        assert!(message.contains("Publish a higher version"), "{message}");
    }

    #[test]
    fn an_image_that_does_not_fit_is_refused_before_anything_is_written() {
        let capacity = capacity_of("usr_b").unwrap();
        let error = plan(Slot::A, "0.1.0.1", &bundle("0.1.0.2", capacity + 1, 1024)).unwrap_err();
        assert_eq!(
            error,
            Refusal::ImageTooLarge {
                size: capacity + 1,
                capacity
            }
        );
        assert!(error.to_string().contains("frozen by ADR-0003"));
    }

    #[test]
    fn an_oversized_verity_tree_is_refused_too() {
        let capacity = capacity_of("usr_b_hash").unwrap();
        let error = plan(Slot::A, "0.1.0.1", &bundle("0.1.0.2", 1024, capacity + 1)).unwrap_err();
        assert!(matches!(error, Refusal::VerityTooLarge { .. }));
    }

    #[test]
    fn the_capacities_come_from_the_frozen_layout_and_not_from_here() {
        // If these ever disagree with ADR-0003, an update fits in this crate's opinion
        // and overruns the partition in reality.
        assert_eq!(capacity_of("usr_a"), Some(1024 * 1024 * 1024));
        assert_eq!(capacity_of("usr_a_hash"), Some(32 * 1024 * 1024));
        assert_eq!(capacity_of("usr_a"), capacity_of("usr_b"));
        assert_eq!(capacity_of("var"), None, "/var takes the remainder");
        assert_eq!(capacity_of("nonexistent"), None);
    }

    #[test]
    fn a_build_stamp_sorts_above_the_version_it_extends() {
        // The format this project publishes: 0.1.0 plus a timestamp. If the appliance
        // and systemd-boot disagreed about this, the updater would write a slot the
        // bootloader declines to choose.
        assert_eq!(
            compare_versions("0.1.0.202607281730", "0.1.0"),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("0.1.0", "0.1.0.202607281730"),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("0.1.0.202607281730", "0.1.0.202607281729"),
            Ordering::Greater
        );
    }

    #[test]
    fn numbers_compare_as_numbers_and_not_as_text() {
        // The mistake that makes 10 older than 9 and is invisible until the tenth build.
        assert_eq!(compare_versions("0.1.10", "0.1.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.2.0", "0.10.0"), Ordering::Less);
        assert_eq!(compare_versions("0.1.0", "0.1.0"), Ordering::Equal);
    }
}
