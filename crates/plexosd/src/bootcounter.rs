//! The boot try counter, which lives in the UKI's filename on the ESP.
//!
//! ADR-0005 chose `systemd-boot`'s boot-counting convention. There is no database and
//! no EFI variable: the counter is part of the entry's name, and the bootloader
//! decrements it by renaming the file before handing off to the kernel. A FAT
//! directory rename is the closest thing to an atomic operation available that early,
//! and it avoids EFI variable writes, which have limited endurance and a history of
//! firmware bugs.
//!
//! ```text
//! plexos-0.2.0+3.efi      3 tries left, none used
//! plexos-0.2.0+2-1.efi    one boot attempted and not yet declared good
//! plexos-0.2.0+0-3.efi    exhausted; the bootloader skips it
//! plexos-0.2.0.efi        marked good; no counter, boots forever
//! ```
//!
//! This module is only the naming. Deciding *when* to mark an entry good is
//! [`crate::health`], and the two are kept apart deliberately: the rule that a boot is
//! good is a policy question with real consequences, and it should not be entangled
//! with string handling.

use std::fmt;

/// A boot entry filename, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootEntry {
    /// Everything before the counter, e.g. `plexos-0.2.0`.
    pub stem: String,
    /// Tries remaining, or `None` when the entry has been marked good.
    pub tries_left: Option<u32>,
    /// Tries already made. Absent until the bootloader has decremented once.
    pub tries_done: Option<u32>,
}

impl BootEntry {
    /// Parses an entry filename. Returns `None` if it is not a `.efi` file.
    ///
    /// A name with no counter is a valid, already-good entry — not an error.
    #[must_use]
    pub fn parse(filename: &str) -> Option<Self> {
        let name = filename.strip_suffix(".efi")?;

        // The counter starts at the last '+', because a version could contain one
        // and the counter is always last.
        let Some((stem, counter)) = name.rsplit_once('+') else {
            return Some(Self {
                stem: name.to_owned(),
                tries_left: None,
                tries_done: None,
            });
        };

        // "+2-1" is two left and one done; "+3" is three left and none done.
        let (left, done) = match counter.split_once('-') {
            Some((left, done)) => (left, Some(done)),
            None => (counter, None),
        };

        let tries_left = left.parse().ok()?;
        let tries_done = match done {
            Some(text) => Some(text.parse().ok()?),
            None => None,
        };

        Some(Self {
            stem: stem.to_owned(),
            tries_left: Some(tries_left),
            tries_done,
        })
    }

    /// Whether this entry still carries a counter, and so is on trial.
    #[must_use]
    pub const fn is_on_trial(&self) -> bool {
        self.tries_left.is_some()
    }

    /// Whether the bootloader has given up on this entry.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.tries_left == Some(0)
    }

    /// The filename this entry takes once the boot is declared good.
    ///
    /// Dropping the counter entirely is what makes the slot permanent; an entry that
    /// merely had its counter reset would be tried three more times and then skipped.
    #[must_use]
    pub fn marked_good(&self) -> String {
        format!("{}.efi", self.stem)
    }
}

impl fmt::Display for BootEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.tries_left, self.tries_done) {
            (None, _) => write!(f, "{}.efi", self.stem),
            (Some(left), None) => write!(f, "{}+{left}.efi", self.stem),
            (Some(left), Some(done)) => write!(f, "{}+{left}-{done}.efi", self.stem),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_entry_has_tries_and_no_attempts() {
        let entry = BootEntry::parse("plexos-0.2.0+3.efi").unwrap();
        assert_eq!(entry.stem, "plexos-0.2.0");
        assert_eq!(entry.tries_left, Some(3));
        assert_eq!(entry.tries_done, None);
        assert!(entry.is_on_trial());
        assert!(!entry.is_exhausted());
    }

    #[test]
    fn an_attempted_entry_records_both_halves() {
        let entry = BootEntry::parse("plexos-0.2.0+2-1.efi").unwrap();
        assert_eq!(entry.tries_left, Some(2));
        assert_eq!(entry.tries_done, Some(1));
    }

    #[test]
    fn an_exhausted_entry_is_recognised() {
        // The bootloader skips these in favour of the other slot. Marking one good
        // would resurrect an image that failed to boot three times.
        let entry = BootEntry::parse("plexos-0.2.0+0-3.efi").unwrap();
        assert!(entry.is_exhausted());
    }

    #[test]
    fn an_entry_with_no_counter_is_already_good() {
        let entry = BootEntry::parse("plexos-0.2.0.efi").unwrap();
        assert_eq!(entry.tries_left, None);
        assert!(!entry.is_on_trial());
        assert_eq!(entry.stem, "plexos-0.2.0");
    }

    #[test]
    fn marking_good_removes_the_counter_rather_than_resetting_it() {
        // Resetting to +3 would leave the entry on trial forever: three more boots,
        // then skipped, on a system that has been working for months.
        let entry = BootEntry::parse("plexos-0.2.0+2-1.efi").unwrap();
        assert_eq!(entry.marked_good(), "plexos-0.2.0.efi");
        assert!(!entry.marked_good().contains('+'));
    }

    #[test]
    fn a_version_containing_a_plus_still_parses() {
        // Build metadata is legal in a semantic version, and the counter is always
        // last. Splitting on the first '+' would read "build" as the counter.
        let entry = BootEntry::parse("plexos-0.2.0+build7+3.efi").unwrap();
        assert_eq!(entry.stem, "plexos-0.2.0+build7");
        assert_eq!(entry.tries_left, Some(3));
        assert_eq!(entry.marked_good(), "plexos-0.2.0+build7.efi");
    }

    #[test]
    fn non_efi_files_are_ignored() {
        assert_eq!(BootEntry::parse("loader.conf"), None);
        assert_eq!(BootEntry::parse("plexos-0.2.0"), None);
    }

    #[test]
    fn a_malformed_counter_is_not_silently_treated_as_good() {
        // "+x" is not a number. Returning an uncounted entry here would let the
        // daemon rename a file it does not understand.
        assert_eq!(BootEntry::parse("plexos-0.2.0+x.efi"), None);
        assert_eq!(BootEntry::parse("plexos-0.2.0+2-y.efi"), None);
    }

    #[test]
    fn display_round_trips_every_form() {
        for name in [
            "plexos-0.2.0.efi",
            "plexos-0.2.0+3.efi",
            "plexos-0.2.0+2-1.efi",
            "plexos-0.2.0+0-3.efi",
        ] {
            assert_eq!(BootEntry::parse(name).unwrap().to_string(), name);
        }
    }
}
