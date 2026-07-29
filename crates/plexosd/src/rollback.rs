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
//! # What has run
//!
//! **Nothing.** No rollback has ever happened on the reference laptop, so this file has
//! never been written by the path that writes it. The shape is exercised by tests only.

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
#[must_use]
pub fn last() -> Option<Record> {
    from_json(&std::fs::read_to_string(paths::ROLLBACK_RECORD_FILE).ok()?)
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
