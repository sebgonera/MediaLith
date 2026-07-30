//! Anti-rollback: refusing an old update that is still validly signed (ADR-0006).
//!
//! A signature says who published something. It says nothing about *when*, and a manifest
//! stays valid forever. Somebody who can answer where this appliance fetches from can
//! therefore serve last month's genuine, correctly signed release — the one with the
//! vulnerability that was fixed since — and every check in the trust chain will pass,
//! because every one of them is true.
//!
//! The counter is what makes that fail. A device remembers the highest `sequence` it has
//! accepted and refuses anything below it. Version strings are for humans; this is the
//! security boundary, and it is deliberately a different field so that nobody is tempted
//! to make a display decision out of it.
//!
//! # The floor is not only what is written down
//!
//! `/var/lib/plexos/update/accepted_sequence` exists only once a machine has taken an
//! update. Every appliance so far was `dd`ed onto a disk, so the file is absent on exactly
//! the machines that have never been protected — and the first update offered to one could
//! be an old one.
//!
//! So the floor is the higher of what is recorded and what the running image *is*: the
//! build stamp inside its own version string is its sequence, by construction (see
//! [`crate::clock::build_stamp`]). A machine can never be talked below the release it is
//! currently executing, with no state at all.
//!
//! # It survives rollback, and that is the point
//!
//! `/var` is the one thing ADR-0005 does not revert. After a rolled-back bad update the
//! floor still names the bad release, so serving it again is refused — the way forward
//! from a failed update is a newer one, which is also the only way that is not a loop.
//!
//! # What has run
//!
//! **Nothing on hardware.** No appliance has yet recorded a sequence, because none has yet
//! installed a signed manifest.

use std::io;
use std::path::Path;

/// Why an update was refused on grounds of age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceError {
    /// The manifest is older than something this device has already accepted.
    Replayed {
        /// What the manifest carries.
        offered: u64,
        /// The lowest sequence this device will accept.
        floor: u64,
    },
    /// The manifest may only be installed on top of a newer release than this one.
    MissingIntermediate {
        /// The floor the manifest demands.
        required: u64,
        /// What this device is at.
        floor: u64,
    },
}

impl std::fmt::Display for SequenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Replayed { offered, floor } => write!(
                f,
                "this update carries sequence {offered} and this appliance has already \
                 accepted {floor}, so it is a downgrade. Remedy: if you did not mean to \
                 publish an old release, treat it as hostile -- a correctly signed old \
                 manifest is exactly what an attacker who cannot forge one would serve. If \
                 you did mean it, publish the change as a new release with a higher \
                 sequence; there is deliberately no way to lower this from the network."
            ),
            Self::MissingIntermediate { required, floor } => write!(
                f,
                "this update may only be installed on top of sequence {required} and this \
                 appliance is at {floor}. Remedy: install the release that carries the \
                 migration this one depends on first, then this one. Skipping it is what \
                 the publisher set min_sequence to prevent."
            ),
        }
    }
}

impl std::error::Error for SequenceError {}

/// The lowest sequence this appliance will accept.
///
/// The higher of what has been recorded and what the running image is. See the module
/// documentation for why the second half is not redundant.
#[must_use]
pub fn floor(recorded: Option<u64>, running_release: &str) -> u64 {
    let running = crate::clock::build_stamp(running_release).unwrap_or(0);
    recorded.unwrap_or(0).max(running)
}

/// Whether a manifest may be installed, on age alone.
///
/// Equal sequences are allowed: reinstalling the release a machine is already running is a
/// thing people do deliberately, and refusing it would protect nothing — the bytes are
/// identical to the ones already on the disk.
///
/// # Errors
/// [`SequenceError`], naming what to do about it.
pub fn check(offered: u64, min_sequence: Option<u64>, floor: u64) -> Result<(), SequenceError> {
    if offered < floor {
        return Err(SequenceError::Replayed { offered, floor });
    }
    if let Some(required) = min_sequence
        && floor < required
    {
        return Err(SequenceError::MissingIntermediate { required, floor });
    }
    Ok(())
}

/// The sequence recorded at `path`, if there is a readable one.
///
/// An unreadable or nonsensical file reads as absent rather than as an error. The
/// alternative is an appliance that refuses every update because a byte on `/var` went
/// bad, and the floor has a second source ([`floor`]) that a corrupt file cannot lower.
#[must_use]
pub fn recorded(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Records `sequence` as accepted, if it is higher than what is already there.
///
/// Never lowers the floor. That is not a convenience: the only thing standing between a
/// replayed manifest and an installed one is this number, so a path that could write a
/// smaller one would be a way to ask the appliance to forget.
///
/// # Errors
/// Anything that stops the write reaching the disk. A caller that cannot record a sequence
/// has installed an update the next boot will not know about, which is worth reporting.
pub fn record(path: &Path, sequence: u64) -> io::Result<()> {
    if recorded(path).is_some_and(|existing| existing >= sequence) {
        return Ok(());
    }

    // A partial write here is a floor of "3" where "202607281844" was meant, which is an
    // appliance that has quietly stopped refusing anything.
    crate::atomic::write(path, format!("{sequence}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("plexos-sequence-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_replayed_manifest_is_refused_even_though_it_is_validly_signed() {
        // The whole reason this exists. Every other check in the chain passes on an old
        // release, because every one of them is true about it.
        let error = check(202_607_281_844, None, 202_607_291_323).unwrap_err();
        assert_eq!(
            error,
            SequenceError::Replayed {
                offered: 202_607_281_844,
                floor: 202_607_291_323,
            }
        );
        let message = error.to_string();
        assert!(message.contains("Remedy:"), "{message}");
        assert!(message.contains("hostile"), "{message}");
    }

    #[test]
    fn reinstalling_what_is_already_running_is_allowed() {
        // Equal, not greater. Refusing this would protect nothing: the bytes are the ones
        // already on the disk, and people reinstall a release deliberately.
        assert!(check(202_607_281_844, None, 202_607_281_844).is_ok());
        assert!(check(202_607_291_323, None, 202_607_281_844).is_ok());
    }

    #[test]
    fn a_machine_with_no_recorded_sequence_is_still_not_downgradable() {
        // Every appliance so far was dd'ed onto a disk and has no such file, which would
        // have made this protection absent on exactly the machines that never had it.
        assert_eq!(floor(None, "0.1.0.202607281844"), 202_607_281_844);
        assert!(check(202_607_271_200, None, floor(None, "0.1.0.202607281844")).is_err());
    }

    #[test]
    fn the_floor_is_the_higher_of_what_is_written_down_and_what_is_running() {
        assert_eq!(floor(Some(300), "0.1.0.202607281844"), 202_607_281_844);
        assert_eq!(
            floor(Some(202_607_291_323), "0.1.0.202607281844"),
            202_607_291_323,
            "a rolled-back machine keeps the floor of the update that failed"
        );
        assert_eq!(
            floor(None, "unknown"),
            0,
            "an unreadable version blocks nothing"
        );
    }

    #[test]
    fn a_mandatory_intermediate_release_cannot_be_skipped() {
        let error = check(202_608_010_000, Some(202_607_300_000), 202_607_281_844).unwrap_err();
        assert!(matches!(error, SequenceError::MissingIntermediate { .. }));
        assert!(error.to_string().contains("Remedy:"));

        // And it is satisfied by being at the floor it names, not above it.
        assert!(check(202_608_010_000, Some(202_607_300_000), 202_607_300_000).is_ok());
    }

    #[test]
    fn a_recorded_sequence_survives_a_write_and_a_read() {
        let dir = scratch("roundtrip");
        let path = dir.join("nested").join("accepted_sequence");

        assert_eq!(recorded(&path), None, "nothing recorded yet");
        record(&path, 202_607_281_844).expect("records");
        assert_eq!(recorded(&path), Some(202_607_281_844));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_never_lowers_the_floor() {
        // A path that could write a smaller number is a way to ask the appliance to forget
        // what it has accepted, which is the one thing standing between a replayed
        // manifest and an installed one.
        let dir = scratch("monotonic");
        let path = dir.join("accepted_sequence");

        record(&path, 202_607_291_323).unwrap();
        record(&path, 202_607_281_844).unwrap();
        assert_eq!(recorded(&path), Some(202_607_291_323));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_record_reads_as_absent_rather_than_as_an_error() {
        // The alternative is an appliance that refuses every update because a byte on /var
        // went bad. The running image's own stamp is the floor that a corrupt file cannot
        // lower.
        let dir = scratch("damaged");
        let path = dir.join("accepted_sequence");
        std::fs::write(&path, b"\0\0not a number").unwrap();

        assert_eq!(recorded(&path), None);
        assert_eq!(
            floor(recorded(&path), "0.1.0.202607281844"),
            202_607_281_844
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_left_behind_by_the_atomic_write() {
        let dir = scratch("atomic");
        let path = dir.join("accepted_sequence");
        record(&path, 1).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["accepted_sequence".to_owned()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
