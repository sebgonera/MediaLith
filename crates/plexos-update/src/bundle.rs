//! What a publisher says about an update, and how little of it is believed.
//!
//! One JSON document beside the artefacts it describes. It is deliberately the same
//! shape as ADR-0006's manifest without any of its guarantees: a version, a root hash,
//! and a list of files with their sizes and digests. When the signed manifest arrives it
//! replaces this type and nothing below it changes.
//!
//! # Why the digests are here at all
//!
//! They do not make the transport trustworthy — whoever serves the bundle serves the
//! digests too. They separate a **truncated download** from a **wrong image**, which need
//! opposite responses: retry the first, never retry the second. The verity root hash is
//! the thing that actually decides whether `/usr` is what it claims to be, and the kernel
//! checks that on every block read for the life of the slot.

use serde::{Deserialize, Serialize};

/// Bundle formats this release understands.
///
/// Refused rather than guessed at, and the number is read before anything else — the same
/// rule ADR-0006 sets for `manifest_version`. A device that does not understand a format
/// must say so and stop, not interpret the parts it recognises.
pub const SUPPORTED_VERSION: u32 = 1;

/// One file in a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// File name, relative to the bundle's own location. Never a path.
    pub name: String,
    /// Size in bytes, checked before anything is written to a partition.
    pub size: u64,
    /// Lower-case hex SHA-256.
    pub sha256: String,
}

impl Artifact {
    /// Whether `name` is a plain file name and not an attempt to escape the bundle.
    ///
    /// The name is joined to a URL and to a path, so `..` or a leading `/` would let a
    /// publisher choose where this reads and writes. Refused as a shape rather than
    /// sanitised, because sanitising is where the interesting bugs live.
    #[must_use]
    pub fn has_safe_name(&self) -> bool {
        !self.name.is_empty()
            && !self.name.contains('/')
            && !self.name.contains('\\')
            && self.name != "."
            && self.name != ".."
            && !self.name.starts_with('.')
    }
}

/// The `/usr` image, its verity tree, and the boot entries that go with them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// Format of this document.
    pub bundle_version: u32,
    /// Human-readable, and the string the bootloader orders entries by. Must increase.
    pub version: String,
    /// The verity root hash of `usr`, which is what the kernel enforces at runtime.
    pub root_hash: String,
    /// The `/usr` filesystem image.
    pub usr: Artifact,
    /// Its dm-verity hash tree.
    pub verity: Artifact,
    /// The Unified Kernel Image for slot A.
    ///
    /// Two, because the kernel command line carries `plexos.slot=`, and the appliance
    /// cannot build a UKI: that needs `objcopy`, which is not in the image and should not
    /// be. Whichever slot the update is written to, the matching entry is installed.
    pub uki_a: Artifact,
    /// The Unified Kernel Image for slot B.
    pub uki_b: Artifact,
}

impl Metadata {
    /// Whether anything vouches for this bundle beyond the network it came from.
    ///
    /// `false`, and a constant rather than a field so that a publisher cannot claim
    /// otherwise. When ADR-0006's signature check exists it will be a property of the
    /// verification, not of the document.
    pub const TRUSTED: bool = false;

    /// Parses a bundle, refusing anything it does not understand.
    ///
    /// # Errors
    /// See [`MetadataError`].
    pub fn parse(document: &str) -> Result<Self, MetadataError> {
        // The version is read first and on its own, so that a future format which
        // renames every other field still produces "I do not understand version N"
        // rather than a parse error listing fields nobody recognises.
        let probe: serde_json::Value = serde_json::from_str(document)
            .map_err(|error| MetadataError::Unreadable(error.to_string()))?;
        let declared = probe
            .get("bundle_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or(MetadataError::NoVersion)?;
        let declared = u32::try_from(declared).unwrap_or(u32::MAX);
        if declared != SUPPORTED_VERSION {
            return Err(MetadataError::UnsupportedVersion(declared));
        }

        let metadata: Self = serde_json::from_str(document)
            .map_err(|error| MetadataError::Unreadable(error.to_string()))?;

        for artifact in metadata.artifacts() {
            if !artifact.has_safe_name() {
                return Err(MetadataError::UnsafeName(artifact.name.clone()));
            }
        }
        if !is_hex(&metadata.root_hash) || metadata.root_hash.len() != 64 {
            return Err(MetadataError::BadRootHash(metadata.root_hash.clone()));
        }
        for artifact in metadata.artifacts() {
            if !is_hex(&artifact.sha256) || artifact.sha256.len() != 64 {
                return Err(MetadataError::BadDigest(artifact.name.clone()));
            }
        }

        Ok(metadata)
    }

    /// Every file this bundle names.
    #[must_use]
    pub fn artifacts(&self) -> [&Artifact; 4] {
        [&self.usr, &self.verity, &self.uki_a, &self.uki_b]
    }

    /// The boot entry for a slot.
    #[must_use]
    pub fn uki_for(&self, slot: plexos_types::Slot) -> &Artifact {
        match slot {
            plexos_types::Slot::A => &self.uki_a,
            plexos_types::Slot::B => &self.uki_b,
        }
    }
}

/// Why a bundle was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    /// Not JSON, or not this shape.
    Unreadable(String),
    /// No `bundle_version` field at all.
    NoVersion,
    /// A format this release does not implement.
    UnsupportedVersion(u32),
    /// A file name that is a path, or hidden.
    UnsafeName(String),
    /// The root hash is not 64 hex characters.
    BadRootHash(String),
    /// An artifact's digest is not 64 hex characters.
    BadDigest(String),
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(detail) => write!(
                f,
                "the update bundle could not be read: {detail}. Either the publisher \
                 wrote something else at that address, or it is not a PlexOS bundle."
            ),
            Self::NoVersion => write!(
                f,
                "the update bundle has no bundle_version, so there is no way to know \
                 what the rest of it means. Nothing was fetched."
            ),
            Self::UnsupportedVersion(found) => write!(
                f,
                "the update bundle is format {found} and this release implements \
                 {SUPPORTED_VERSION}. Refusing rather than interpreting the parts that \
                 look familiar. Update this appliance from a bundle it understands, or \
                 rebuild the publisher."
            ),
            Self::UnsafeName(name) => write!(
                f,
                "the update bundle names a file as {name:?}, which is a path rather than \
                 a name. A publisher does not get to choose where this appliance reads \
                 and writes, so the whole bundle is refused."
            ),
            Self::BadRootHash(hash) => write!(
                f,
                "the update bundle's root hash {hash:?} is not 64 hex characters. \
                 dm-verity would refuse it at boot, which is far too late to find out."
            ),
            Self::BadDigest(name) => write!(
                f,
                "the digest for {name:?} is not 64 hex characters, so nothing downloaded \
                 could be checked against it."
            ),
        }
    }
}

impl std::error::Error for MetadataError {}

/// Whether every character is a lower-case hex digit.
fn is_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "b024b422b89fe9c8bd140915b3633c0819c183f83b45fc26b884d1d4971d2aa7";
    const DIGEST: &str = "41e158cca7e7182c0bec7abc52be83ce3d12e0db31e7aedf9d648671a71ea5f3";

    fn document() -> String {
        format!(
            r#"{{
              "bundle_version": 1,
              "version": "0.1.0.202607281730",
              "root_hash": "{HASH}",
              "usr":    {{ "name": "usr.erofs", "size": 74448896, "sha256": "{DIGEST}" }},
              "verity": {{ "name": "usr.hash",  "size": 1179648,  "sha256": "{DIGEST}" }},
              "uki_a":  {{ "name": "plexos-a.efi", "size": 18973184, "sha256": "{DIGEST}" }},
              "uki_b":  {{ "name": "plexos-b.efi", "size": 18973184, "sha256": "{DIGEST}" }}
            }}"#
        )
    }

    #[test]
    fn a_well_formed_bundle_parses() {
        let metadata = Metadata::parse(&document()).unwrap();
        assert_eq!(metadata.version, "0.1.0.202607281730");
        assert_eq!(metadata.root_hash, HASH);
        assert_eq!(metadata.usr.name, "usr.erofs");
        assert_eq!(metadata.artifacts().len(), 4);
    }

    #[test]
    fn each_slot_gets_its_own_boot_entry() {
        // The command line carries plexos.slot=, and the appliance cannot build a UKI --
        // that needs objcopy, which is not in the image. So the publisher ships both and
        // the updater installs whichever matches the slot it wrote.
        let metadata = Metadata::parse(&document()).unwrap();
        assert_eq!(metadata.uki_for(plexos_types::Slot::A).name, "plexos-a.efi");
        assert_eq!(metadata.uki_for(plexos_types::Slot::B).name, "plexos-b.efi");
    }

    #[test]
    fn an_unknown_format_is_refused_rather_than_partly_understood() {
        // ADR-0006's rule, applied to its stand-in: read the version first, and stop.
        let future = document().replace("\"bundle_version\": 1", "\"bundle_version\": 9");
        let error = Metadata::parse(&future).unwrap_err();
        assert_eq!(error, MetadataError::UnsupportedVersion(9));
        assert!(
            error
                .to_string()
                .contains("Refusing rather than interpreting")
        );
    }

    #[test]
    fn a_document_with_no_version_is_refused_before_its_fields_are_read() {
        let anonymous = document().replace("\"bundle_version\": 1,", "");
        assert_eq!(Metadata::parse(&anonymous), Err(MetadataError::NoVersion));
    }

    #[test]
    fn a_publisher_cannot_choose_where_this_reads_and_writes() {
        // The name is joined to a URL and to a path. A publisher that could put ".." or
        // an absolute path in it would choose both.
        for hostile in ["../../etc/passwd", "/etc/passwd", "a/b", ".hidden", ""] {
            let tampered =
                document().replace("\"name\": \"usr.erofs\"", &format!("\"name\": {hostile:?}"));
            assert!(
                matches!(
                    Metadata::parse(&tampered),
                    Err(MetadataError::UnsafeName(_))
                ),
                "{hostile:?} must be refused"
            );
        }
    }

    #[test]
    fn a_root_hash_that_verity_would_reject_is_refused_now_rather_than_at_boot() {
        // dm-verity would refuse it when the slot is booted, which is three reboots and
        // a rollback later than finding out here.
        for bad in ["", "not-a-hash", "ABCD", &HASH[..63]] {
            let tampered = document().replace(HASH, bad);
            assert!(
                matches!(
                    Metadata::parse(&tampered),
                    Err(MetadataError::BadRootHash(_))
                ),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn nothing_in_the_bundle_claims_to_be_trusted() {
        // A constant rather than a field, so a publisher cannot assert otherwise, and so
        // that the day signatures arrive this stops compiling in the places that assume
        // it.
        const { assert!(!Metadata::TRUSTED) };
    }

    #[test]
    fn uppercase_hex_is_refused_because_two_spellings_of_one_hash_is_one_too_many() {
        let shouted = document().replace(HASH, &HASH.to_ascii_uppercase());
        assert!(matches!(
            Metadata::parse(&shouted),
            Err(MetadataError::BadRootHash(_))
        ));
    }
}
