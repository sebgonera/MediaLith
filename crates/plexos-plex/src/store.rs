//! The app image store: which versions are on disk, which one runs, what gets deleted.
//!
//! ADR-0007 puts Plex under `/var/lib/plexos/apps/plex` as version-named, immutable
//! image files with a `current` symlink:
//!
//! ```text
//! 1.42.2.10156.img
//! 1.43.3.10828.img
//! current -> 1.43.3.10828.img
//! ```
//!
//! Updating is a download, an atomic symlink swap and a restart. Rolling back is moving
//! the symlink — the same shape as an OS rollback, at a different granularity.
//!
//! # Why the integrity record is a `sha256sum` file and not a schema
//!
//! ADR-0010 requires the installed artefact's hash to be recorded, because the
//! signature that vouched for it used MD5 and SHA1 and is only checked once, at
//! provisioning. Storing it means writing to `/var`, whose layout is frozen: ADR-0009
//! allows a migration only to *add*, and anything a previous release cannot read is a
//! rollback hazard — and rollback reverts `/usr`, never `/var`.
//!
//! So this deliberately introduces no format. The record is a sidecar in the exact
//! output format of `sha256sum(1)`, which every release can read, `sha256sum -c` can
//! check by hand, and no parser of ours can ever be wrong about. A release that has
//! never heard of it simply sees a file it ignores.
//!
//! # What this module does and does not touch
//!
//! Pure decisions over lists of names: what to keep, what to remove, what `current`
//! should point at. Nothing here opens a file. The caller performs the filesystem work,
//! which keeps every rule about retention and ordering testable without a disk.

use std::cmp::Ordering;

/// Extension of an app image.
pub const IMAGE_EXT: &str = ".img";

/// Name of the symlink naming the active image.
pub const CURRENT_LINK: &str = "current";

/// How many images to keep, counting the current one.
///
/// ADR-0007: the current image and one previous. Retention is a runtime decision
/// precisely so it is not frozen into the partition table, but two is the documented
/// policy and changing it is an ADR change, not an edit here.
pub const KEEP: usize = 2;

/// An upstream Plex version, as it appears in a package name.
///
/// Plex writes `1.43.3.10828-00f62d37d`: four numeric components and a build hash. The
/// hash is kept for display and ignored when ordering, because it says nothing about
/// which release is newer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The numeric components, most significant first.
    pub parts: Vec<u64>,
    /// The full string as upstream wrote it, including any build suffix.
    pub raw: String,
}

impl Version {
    /// Parses a version, keeping the original text.
    ///
    /// Returns `None` when there is no leading numeric component, which is what
    /// distinguishes a version from a stray filename in the same directory.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let numeric = raw.split('-').next().unwrap_or(raw);
        let parts: Vec<u64> = numeric
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        if parts.is_empty() {
            return None;
        }
        Some(Self {
            parts,
            raw: raw.to_owned(),
        })
    }

    /// The file name this version's image has.
    #[must_use]
    pub fn image_name(&self) -> String {
        format!("{}{IMAGE_EXT}", self.raw)
    }

    /// The file name of its integrity record.
    #[must_use]
    pub fn record_name(&self) -> String {
        format!("{}{IMAGE_EXT}.sha256", self.raw)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // Component-wise, and a missing component counts as zero so that 1.43 sorts
        // below 1.43.1 rather than beside it. Comparing the strings instead puts
        // 1.43.3.9999 above 1.43.3.10828, because "9" > "1" — which would retain the
        // wrong image and roll a user forward onto an older Plex.
        let width = self.parts.len().max(other.parts.len());
        for i in 0..width {
            let a = self.parts.get(i).copied().unwrap_or(0);
            let b = other.parts.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        Ordering::Equal
    }
}

/// Reads a version back out of an image file name.
#[must_use]
pub fn version_of(file_name: &str) -> Option<Version> {
    Version::parse(file_name.strip_suffix(IMAGE_EXT)?)
}

/// Everything the store holds, derived from a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    /// Installed versions, oldest first.
    pub installed: Vec<Version>,
    /// What `current` points at, if it points at anything recognisable.
    pub current: Option<Version>,
}

impl Store {
    /// Reads the store from a directory listing and the symlink's target.
    ///
    /// Names that are not images are ignored rather than rejected: partial downloads
    /// and an operator's stray copy both land here, and refusing to describe the store
    /// because of one would take Plex down over a file nobody is using.
    #[must_use]
    pub fn from_listing(entries: &[String], current_target: Option<&str>) -> Self {
        let mut installed: Vec<Version> = entries.iter().filter_map(|e| version_of(e)).collect();
        installed.sort();
        Self {
            current: current_target.and_then(version_of),
            installed,
        }
    }

    /// Which images to delete after `keep_current` has become the active version.
    ///
    /// Keeps the active image and the [`KEEP`] − 1 newest others. "Newest" is by version
    /// rather than by install time, which gives the right answer in both directions: on
    /// an upgrade the previous release is the newest other, and after a deliberate
    /// downgrade the one just rolled back from is too.
    ///
    /// The active image is never a candidate, whatever its version. Deleting the file
    /// `current` points at leaves a dangling symlink and a Plex that cannot start, and
    /// no retention policy is worth that.
    #[must_use]
    pub fn superseded(&self, keep_current: &Version) -> Vec<Version> {
        let mut others: Vec<&Version> = self
            .installed
            .iter()
            .filter(|v| *v != keep_current)
            .collect();
        // Newest first, so the tail is what goes.
        others.sort_by(|a, b| b.cmp(a));
        others
            .into_iter()
            .skip(KEEP.saturating_sub(1))
            .cloned()
            .collect()
    }

    /// Is this version already installed?
    #[must_use]
    pub fn holds(&self, version: &Version) -> bool {
        self.installed.contains(version)
    }
}

/// The body of an integrity record, in `sha256sum(1)` format.
///
/// Two spaces between digest and name, which is what the tool writes and what
/// `sha256sum -c` expects. One space means "text mode" to some implementations and a
/// parse failure in others, so the difference is not cosmetic.
#[must_use]
pub fn record_body(sha256: &str, image_name: &str) -> String {
    format!("{sha256}  {image_name}\n")
}

/// Reads a digest back out of a record, checking it refers to the expected image.
///
/// Returns `None` when the record names a different file, which happens if an image is
/// renamed by hand: the digest would then be checked against the wrong bytes and either
/// fail confusingly or, worse, pass.
#[must_use]
pub fn digest_from_record(body: &str, image_name: &str) -> Option<String> {
    let line = body.lines().next()?;
    let (digest, named) = line.split_once("  ")?;
    if named.trim() != image_name {
        return None;
    }
    let digest = digest.trim();
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(digest.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(raw: &str) -> Version {
        Version::parse(raw).expect("a version")
    }

    #[test]
    fn plexs_real_version_strings_parse() {
        let parsed = v("1.43.3.10828-00f62d37d");
        assert_eq!(parsed.parts, [1, 43, 3, 10828]);
        assert_eq!(parsed.raw, "1.43.3.10828-00f62d37d");
        assert_eq!(parsed.image_name(), "1.43.3.10828-00f62d37d.img");
    }

    #[test]
    fn versions_order_numerically_and_not_as_text() {
        // The one that matters. As strings, "1.43.3.9999" > "1.43.3.10828", because '9'
        // beats '1'. Plex's build numbers cross that boundary constantly, so a
        // string sort silently retains the wrong image and can downgrade a user.
        assert!(v("1.43.3.10828") > v("1.43.3.9999"));
        assert!(v("1.43.3.9999") < v("1.43.3.10828"));
        assert!(v("1.42.2.10156") < v("1.43.3.10828"));
    }

    #[test]
    fn a_shorter_version_is_padded_rather_than_treated_as_greater() {
        assert!(v("1.43") < v("1.43.1"));
        assert_eq!(v("1.43").cmp(&v("1.43.0")), Ordering::Equal);
    }

    #[test]
    fn the_build_hash_does_not_affect_ordering() {
        assert_eq!(
            v("1.43.3.10828-00f62d37d").cmp(&v("1.43.3.10828-ffffffff")),
            Ordering::Equal
        );
    }

    #[test]
    fn things_that_are_not_versions_are_not_versions() {
        assert!(Version::parse("current").is_none());
        assert!(Version::parse("").is_none());
        assert!(Version::parse("1.43.x.1").is_none());
        assert!(version_of("1.43.3.10828.img.part").is_none());
        assert!(version_of("notes.txt").is_none());
    }

    #[test]
    fn a_store_is_read_from_a_listing_with_junk_in_it() {
        // A partial download and a stray file. Refusing to describe the store because
        // of either would take Plex down over a file nothing is using.
        let entries = [
            "1.42.2.10156.img".to_owned(),
            "1.43.3.10828.img".to_owned(),
            "1.43.4.11000.img.part".to_owned(),
            "current".to_owned(),
            "notes.txt".to_owned(),
        ];
        let store = Store::from_listing(&entries, Some("1.43.3.10828.img"));
        assert_eq!(store.installed.len(), 2);
        assert_eq!(store.installed[0], v("1.42.2.10156"), "oldest first");
        assert_eq!(store.current, Some(v("1.43.3.10828")));
    }

    #[test]
    fn a_fresh_install_has_nothing_to_delete() {
        let store = Store::from_listing(&["1.43.3.10828.img".to_owned()], None);
        assert_eq!(store.superseded(&v("1.43.3.10828")), []);
    }

    #[test]
    fn upgrading_keeps_the_new_image_and_one_previous() {
        let entries = [
            "1.41.0.9000.img".to_owned(),
            "1.42.2.10156.img".to_owned(),
            "1.43.3.10828.img".to_owned(),
        ];
        let store = Store::from_listing(&entries, Some("1.43.3.10828.img"));
        assert_eq!(store.superseded(&v("1.43.3.10828")), [v("1.41.0.9000")]);
    }

    #[test]
    fn the_active_image_is_never_deleted_however_old_it_is() {
        // After a deliberate downgrade the active image is the oldest on disk.
        // Retention by age alone would delete the running Plex and leave `current`
        // dangling, which is a broken machine rather than a tidy one.
        let entries = [
            "1.41.0.9000.img".to_owned(),
            "1.42.2.10156.img".to_owned(),
            "1.43.3.10828.img".to_owned(),
        ];
        let store = Store::from_listing(&entries, Some("1.41.0.9000.img"));
        let going = store.superseded(&v("1.41.0.9000"));
        assert!(!going.contains(&v("1.41.0.9000")), "{going:?}");
        assert_eq!(going, [v("1.42.2.10156")], "the newest other is kept");
    }

    #[test]
    fn a_reinstall_of_the_running_version_removes_nothing() {
        let entries = ["1.43.3.10828.img".to_owned(), "1.42.2.10156.img".to_owned()];
        let store = Store::from_listing(&entries, Some("1.43.3.10828.img"));
        assert!(store.holds(&v("1.43.3.10828")));
        assert_eq!(store.superseded(&v("1.43.3.10828")), []);
    }

    #[test]
    fn the_record_is_exactly_what_sha256sum_writes() {
        // Two spaces. `sha256sum -c` on the appliance has to be able to check this by
        // hand, and one space is a different format to some implementations.
        let body = record_body(&"a".repeat(64), "1.43.3.10828.img");
        assert_eq!(body, format!("{}  1.43.3.10828.img\n", "a".repeat(64)));
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn a_record_round_trips() {
        let digest = "d627a1eea7355014e8aea4132944202b333de84b5a29967c1d8abd20b7fe5f73";
        let body = record_body(digest, "1.42.2.10156.img");
        assert_eq!(
            digest_from_record(&body, "1.42.2.10156.img").as_deref(),
            Some(digest)
        );
    }

    #[test]
    fn a_record_naming_a_different_image_is_refused() {
        // An image renamed by hand. Accepting the digest would check it against bytes
        // it was never computed from, and the result is either a baffling failure or a
        // pass that means nothing.
        let body = record_body(&"a".repeat(64), "1.42.2.10156.img");
        assert_eq!(digest_from_record(&body, "1.43.3.10828.img"), None);
    }

    #[test]
    fn a_malformed_digest_is_refused_rather_than_compared() {
        assert_eq!(digest_from_record("short  x.img\n", "x.img"), None);
        assert_eq!(digest_from_record("zz  x.img\n", "x.img"), None);
        assert_eq!(
            digest_from_record(&format!("{} x.img\n", "a".repeat(64)), "x.img"),
            None,
            "one space is not the format sha256sum writes"
        );
    }
}
