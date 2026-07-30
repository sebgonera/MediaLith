//! Checking that a downloaded boot entry is the one this slot needs.
//!
//! The manifest says which of its two Unified Kernel Images belongs to which slot, and
//! ADR-0006's schema says the updater checks that the verity root hash inside the UKI
//! matches the one the manifest declares. This is that check, done on the bytes rather
//! than on the claim.
//!
//! # Why it is worth doing after the digest already matched
//!
//! Because the digest proves the file is the one the manifest named, and this proves the
//! manifest named the right one. They are different mistakes and only one of them is an
//! attack: the likely cause here is a publishing script that copied slot A's image into
//! both fields, which no signature and no digest can notice.
//!
//! The two failures that produces are both quiet. Writing slot B and booting an entry that
//! says `plexos.slot=a` mounts the slot the machine was already running — an update that
//! installs, reboots, and changes nothing. And a root hash from a different build makes
//! dm-verity refuse the slot at boot, which reads as a corrupt download and costs three
//! reboots to find out otherwise.
//!
//! # What has run
//!
//! **Nothing on hardware.** `post-image.sh` performs the same check on the build host with
//! `strings`, and has caught nothing yet because nothing has been wrong there.

use plexos_types::Slot;

/// Why a boot entry does not belong to the slot it was fetched for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UkiError {
    /// The image carries a different `plexos.slot=`, or none at all.
    WrongSlot {
        /// The slot it was fetched for.
        wanted: Slot,
        /// What its command line says, if anything recognisable.
        found: Option<Slot>,
    },
    /// The image does not carry the root hash the manifest declares.
    RootHashMismatch {
        /// The hash the manifest declares for this update.
        expected: String,
    },
}

impl std::fmt::Display for UkiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongSlot { wanted, found } => write!(
                f,
                "the boot entry published for slot {wanted} has {} on its command line. \
                 Remedy: republish the bundle; its two kernel images were built or copied \
                 wrongly. Nothing was written. Installing it would have produced a machine \
                 that reboots into the slot it was already running, which looks exactly \
                 like an update that did nothing.",
                match found {
                    Some(slot) => format!("plexos.slot={slot}"),
                    None => "no plexos.slot at all".to_owned(),
                }
            ),
            Self::RootHashMismatch { expected } => write!(
                f,
                "the boot entry does not carry the verity root hash {expected} that this \
                 update declares, so it belongs to a different build. Remedy: republish \
                 the bundle. Nothing was written; installing it would have failed \
                 dm-verity at boot and looked like a corrupt download."
            ),
        }
    }
}

impl std::error::Error for UkiError {}

/// Whether `image` is the boot entry for `slot`, carrying `root_hash`.
///
/// Reads the command line out of the bytes rather than parsing PE sections. The command
/// line is plain ASCII in a `.cmdline` section and this is looking for two literal
/// substrings; a PE parser here would be a lot of code whose failure mode is refusing a
/// perfectly good image.
///
/// # Errors
/// [`UkiError`], naming what to republish.
pub fn check(image: &[u8], slot: Slot, root_hash: &str) -> Result<(), UkiError> {
    let says = |s: Slot| contains(image, format!("plexos.slot={s}").as_bytes());

    if !says(slot) {
        return Err(UkiError::WrongSlot {
            wanted: slot,
            found: Slot::ALL.into_iter().find(|other| says(*other)),
        });
    }

    if !contains(image, root_hash.as_bytes()) {
        return Err(UkiError::RootHashMismatch {
            expected: root_hash.to_owned(),
        });
    }

    Ok(())
}

/// Whether `haystack` contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "b024b422b89fe9c8bd140915b3633c0819c183f83b45fc26b884d1d4971d2aa7";

    /// Bytes shaped like a UKI: a command line buried in padding.
    fn image(slot: &str, hash: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; 512];
        bytes.extend_from_slice(
            format!("plexos.slot={slot} plexos.roothash={hash} panic=20 console=tty0\n").as_bytes(),
        );
        bytes.extend_from_slice(&[0u8; 512]);
        bytes
    }

    #[test]
    fn the_entry_for_a_slot_is_accepted() {
        for slot in Slot::ALL {
            assert!(check(&image(&slot.to_string(), HASH), slot, HASH).is_ok());
        }
    }

    #[test]
    fn the_other_slots_entry_is_refused_and_says_which_it_was() {
        // The mistake a publishing script makes: slot A's image copied into both fields.
        // Neither the digest nor the signature can see it, because both are true.
        let error = check(&image("a", HASH), Slot::B, HASH).unwrap_err();
        assert_eq!(
            error,
            UkiError::WrongSlot {
                wanted: Slot::B,
                found: Some(Slot::A),
            }
        );
        let message = error.to_string();
        assert!(message.contains("plexos.slot=a"), "{message}");
        assert!(message.contains("did nothing"), "{message}");
        assert!(message.contains("Remedy:"), "{message}");
    }

    #[test]
    fn an_image_with_no_slot_at_all_is_refused_without_guessing() {
        let error = check(b"not a kernel image", Slot::A, HASH).unwrap_err();
        assert_eq!(
            error,
            UkiError::WrongSlot {
                wanted: Slot::A,
                found: None,
            }
        );
        assert!(error.to_string().contains("no plexos.slot at all"));
    }

    #[test]
    fn an_entry_from_another_build_is_refused_before_it_can_fail_verity() {
        // Otherwise this costs three reboots and reads as a corrupt download, which sends
        // somebody to re-run the transfer that was never the problem.
        let other = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let error = check(&image("a", other), Slot::A, HASH).unwrap_err();
        assert!(matches!(error, UkiError::RootHashMismatch { .. }));
        assert!(error.to_string().contains(HASH));
    }

    #[test]
    fn an_empty_image_matches_nothing() {
        assert!(check(&[], Slot::A, HASH).is_err());
        assert!(!contains(b"", b"x"));
        assert!(!contains(b"short", b"much longer needle"));
    }
}
