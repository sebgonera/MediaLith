//! Writing a small file so that a machine losing power mid-write finds one of the two
//! versions and never half of either.
//!
//! Everything this crate persists is state that decides what the appliance will accept
//! next: the anti-rollback floor and the revocation list. A truncated `202607281844` is
//! `2026`, which is a floor that refuses nothing, and a truncated revocation list is a
//! list that revokes nothing. Both failures are silent and both are in the permissive
//! direction, which is the combination worth spending a rename on.
//!
//! Power going off mid-write is not a remote possibility here. The whole point of an
//! update is that the machine restarts, and the person doing it is often holding the
//! power button because something has gone wrong.

use std::io;
use std::path::Path;

/// Writes `bytes` to `path`, replacing what was there, or leaves it untouched.
///
/// Creates the parent directory if it is missing, which is the normal case on a `/var`
/// that has never taken an update.
///
/// # Errors
/// Anything that stops the bytes reaching the disk.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Beside the target, so the rename is within one filesystem and therefore atomic.
    // `sync_all` before the rename, or the rename can reach the disk before the contents
    // and a crash leaves an empty file where a valid one used to be.
    let temporary = path.with_extension("new");
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("plexos-atomic-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_through_a_missing_directory_and_replaces_what_was_there() {
        let dir = scratch("replace");
        let path = dir.join("nested").join("state");

        write(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_no_temporary_behind() {
        // A leftover would be harmless in itself and would be read by nothing -- but on an
        // appliance nobody logs into, a directory that accumulates files is a directory
        // whose contents stop being evidence of anything.
        let dir = scratch("tidy");
        write(&dir.join("state"), b"x").unwrap();

        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["state".to_owned()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
