//! A note that outlives the image it is about.
//!
//! ADR-0005 hands a machine that fails three boots back to the previous slot, and then
//! the previous slot has no idea why it is running. Everything that could explain the
//! failure — the log, the gate's verdict, the version string, the boot entry — lived in
//! the `/usr` that was just rolled away. What a person sees is an appliance that quietly
//! went backwards a version, with a healthy status page and nothing to read.
//!
//! `/var` is the one surface that survives, and it survives *because* of the same rule:
//! rollback reverts `/usr` and never `/var` (ADR-0005, ADR-0009). The property that makes
//! `/var` awkward for migrations is exactly what makes it the right place for this.
//!
//! # Written before the restart, not after
//!
//! There is no "after" — [`crate::power::stop_now`] does not return. So the record is
//! written while the machine is still healthy enough to write, before Plex is stopped and
//! before `/var` is remounted read-only.
//!
//! # It is history, not status
//!
//! Nothing clears this file. That is deliberate: a lifecycle needs somebody to decide when
//! a rollback stops being true, and every answer is wrong somewhere — clearing on the next
//! update hides a repeat of the same failure, clearing on a healthy boot erases it before
//! anyone reads it, since the boot that reads it is healthy by definition. So it is
//! phrased and served as *the last rollback*, carrying the version it was about, and a
//! reader compares that against what is running now.
//!
//! **That last sentence was a promise nobody kept.** Every reader served the record raw,
//! so a rollback from 1 August was still being announced on 10 August, in the future
//! tense, by a machine several releases past it and healthy on a permanent slot. The
//! comparison now lives in [`last_for`] rather than in each caller's good intentions —
//! which is the only version of "a reader compares" that survives having more than one
//! reader. The file is still never cleared, and that part was right.
//!
//! # What has run
//!
//! **A rollback has happened on the reference laptop**, and this file is how anyone knows:
//! `0.1.0.202608010950` failed its health gate on slot b on 1 August, spent its last try,
//! and the machine came back on the release below it. The record it left is the one that
//! then over-stayed by nine days.

use std::path::Path;

use plexos_types::paths;

/// Why the previous boot was handed back to the other slot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// The version that failed. Compared against the running one by whoever reads this.
    pub version: Option<String>,
    /// The slot it failed on, `a` or `b`.
    pub slot: Option<String>,
    /// Tries remaining after the boot that wrote this. `0` is the one that changed slots.
    pub tries_left: u32,
    /// The health checks that failed, each as `name: detail`.
    pub failures: Vec<String>,
    /// The gate's whole verdict, as it was logged.
    pub verdict: String,
}

/// Renders a record as pretty JSON.
///
/// # Errors
/// Fails only if serialisation does, which for this shape means an allocation failure.
pub fn to_json(record: &Record) -> serde_json::Result<String> {
    serde_json::to_string_pretty(record)
}

/// Parses a record. Anything unreadable is `None` rather than an error.
///
/// A truncated file — power lost mid-write, which is a thing that happens to the class of
/// machine that writes this — must not blank the status page that is the only way to find
/// out anything at all.
#[must_use]
pub fn from_json(contents: &str) -> Option<Record> {
    serde_json::from_str(contents).ok()
}

/// Writes the record, creating the update directory if the image never has.
///
/// # Errors
/// Any I/O failure, so the caller can say so. It must not abort the restart: a rollback
/// that happens without a note is worse than nothing being written, but a rollback that
/// does not happen because a note could not be written is worse than both.
pub fn write(record: &Record, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = to_json(record).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// Reads the last rollback, if there has ever been one.
///
/// History, unfiltered. Almost every caller wants [`last_for`] instead: this one keeps
/// answering with a rollback from a fortnight ago, which is exactly how a status page ends
/// up announcing an emergency that is over.
#[must_use]
pub fn last() -> Option<Record> {
    from_json(&std::fs::read_to_string(paths::ROLLBACK_RECORD_FILE).ok()?)
}

/// The last rollback, but only while it still describes the machine that is running.
///
/// The comparison this module's header says a reader has to make, made in one place
/// instead of left to each of them. It was left to each of them, and none of them made it:
/// the console served the record raw, so an appliance that was rolled back on 1 August
/// still told anyone who looked, nine days and several releases later, that
/// "the slot will roll back" — in the future tense, on a healthy machine whose slot the
/// gate had long since called permanent.
///
/// That is worse than untidy. This is the one line that would say a rollback is happening
/// *now*, and a line that is always on is a line nobody reads — the argument `setup`
/// already makes about banners, arriving here in a worse place.
#[must_use]
pub fn last_for(running: &str) -> Option<Record> {
    let record = last()?;
    record.still_current(running).then_some(record)
}

impl Record {
    /// Whether this rollback still describes the running system.
    ///
    /// A rollback leaves the machine on something *older* than the version that failed, so
    /// while the record is current the running stamp is below the recorded one. Installing
    /// anything from that version onwards is the operator moving past it, and from then on
    /// this is history.
    ///
    /// Undecidable means current. If either version carries no build stamp there is no
    /// ordering to reason about, and the two mistakes are not equal: showing a rollback
    /// that is over costs a stale line, hiding one that is not costs the only warning the
    /// machine gives.
    #[must_use]
    pub fn still_current(&self, running: &str) -> bool {
        // The same parser the anti-rollback floor uses, deliberately. Its own
        // documentation makes the argument: two readers of this field are two chances to
        // disagree about which release a machine is running.
        let Some(failed) = self
            .version
            .as_deref()
            .and_then(plexos_update::clock::build_stamp)
        else {
            return true;
        };
        let Some(running) = plexos_update::clock::build_stamp(running) else {
            return true;
        };
        running < failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_record() -> Record {
        Record {
            version: Some("0.1.0.202607291200".to_owned()),
            slot: Some("a".to_owned()),
            tries_left: 2,
            failures: vec!["plex-http: not answering".to_owned()],
            verdict: "NOT healthy, and this entry is on trial with 2 tries left".to_owned(),
        }
    }

    #[test]
    fn a_record_survives_the_round_trip() {
        let json = to_json(&a_record()).expect("serialises");
        assert_eq!(from_json(&json), Some(a_record()));
    }

    #[test]
    fn a_truncated_file_reads_as_no_record_rather_than_an_error() {
        // Power lost mid-write is a normal event for a machine whose whole subject is
        // failing boots. The status page must still render.
        let json = to_json(&a_record()).expect("serialises");
        let cut = &json[..json.len() / 2];
        assert_eq!(from_json(cut), None);
        assert_eq!(from_json(""), None);
    }

    #[test]
    fn the_recorded_version_is_comparable_to_the_one_the_update_api_reports() {
        // The page distinguishes "you were rolled back off this version" from "an older
        // boot of what you are running failed once", and it does that by comparing these
        // two strings. They must come from one source, so this pins that they do: both
        // are VERSION_ID out of /etc/os-release. A record that read its version from the
        // boot entry or the command line instead would compare unequal to itself.
        let os_release = "ID=plexos\nVERSION_ID=0.1.0.202607291200\n";
        assert_eq!(
            crate::status::os_release_value(os_release, "VERSION_ID"),
            a_record().version
        );
        assert_eq!(
            crate::update::running_version_from(os_release),
            a_record().version.unwrap()
        );
    }

    /// A record of the rollback that actually happened, on 2026-08-01.
    fn the_real_one() -> Record {
        Record {
            version: Some("0.1.0.202608010950".to_owned()),
            slot: Some("b".to_owned()),
            tries_left: 0,
            failures: vec![
                "plex-http: installed but not answering on loopback; the slot will roll back"
                    .to_owned(),
            ],
            verdict: "NOT healthy, and this entry's last try was spent booting it".to_owned(),
        }
    }

    #[test]
    fn a_rollback_is_current_while_the_machine_sits_on_the_release_below_it() {
        // Immediately after the event, which is the whole reason the file exists: the
        // running system is the older one, and nothing else can explain why.
        assert!(the_real_one().still_current("0.1.0.202608010920"));
    }

    #[test]
    fn a_rollback_is_history_once_something_newer_is_running() {
        // The defect, with the numbers it had on the machine. The appliance was rolled
        // back on 1 August, ran 0.1.0.202608102030 nine days later, and was still being
        // told "the slot will roll back" -- in the future tense, on a slot the gate had
        // called permanent. The operator moving past the version that failed is what ends
        // this, and nothing else can.
        assert!(!the_real_one().still_current("0.1.0.202608102030"));
    }

    #[test]
    fn the_version_that_failed_running_again_is_history_too() {
        // It was reinstalled and it works. Equal, not merely greater, because a record
        // describing the release currently serving the page has been overtaken by events
        // in the plainest possible way.
        assert!(!the_real_one().still_current("0.1.0.202608010950"));
    }

    #[test]
    fn a_version_with_no_build_stamp_leaves_the_record_standing() {
        // Undecidable resolves towards showing it. The two mistakes are not equal: a
        // stale line costs a reader ten seconds, a suppressed one costs them the only
        // warning this machine gives that it is failing boots.
        assert!(the_real_one().still_current("0.1.0"));

        let unstamped = Record {
            version: Some("0.1.0".to_owned()),
            ..the_real_one()
        };
        assert!(unstamped.still_current("0.1.0.202608102030"));

        let none = Record {
            version: None,
            ..the_real_one()
        };
        assert!(none.still_current("0.1.0.202608102030"));
    }

    #[test]
    fn writing_creates_the_directory_the_image_may_never_have() {
        // /var/lib/plexos/update exists on a machine that has updated. This writes on a
        // machine that has failed, which is not the same set.
        let dir = std::env::temp_dir().join("plexos-rollback-write");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("never/made/rollback.json");

        write(&a_record(), &path).expect("writes");

        let read = from_json(&std::fs::read_to_string(&path).expect("reads"));
        assert_eq!(read, Some(a_record()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_record_lives_on_var_because_var_is_what_rollback_leaves_alone() {
        // The whole reason this module exists. A path under /usr would be reverted by the
        // event it is describing, and a path under /run would not survive the restart.
        assert!(
            paths::ROLLBACK_RECORD_FILE.starts_with(paths::VAR),
            "rollback reverts /usr and never /var (ADR-0005); a record anywhere else \
             disappears exactly when it becomes interesting"
        );
    }
}
