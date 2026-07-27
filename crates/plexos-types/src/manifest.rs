//! The update manifest (ADR-0006).
//!
//! The manifest is the only document a deployed device parses before it can be told
//! anything new, which makes it the format with the least room for error in the whole
//! system. Three properties are enforced structurally here rather than left to the
//! updater to remember:
//!
//! **Signatures cover bytes, not structures.** [`RawManifest`] owns the bytes that
//! were fetched, and [`RawManifest::signed_bytes`] returns exactly those bytes.
//! [`Manifest`] deliberately has no serialisation path that claims to reproduce them.
//! JSON canonicalisation is a well-worn source of signature-bypass bugs, and the only
//! reliable defence is to never re-serialise a document you intend to verify.
//!
//! **The version is parsed before the body.** [`RawManifest::parse`] reads
//! `manifest_version` through [`VersionProbe`] and refuses an unsupported version
//! before attempting anything else, so a v3 manifest reaching a v0.1 device produces a
//! clear diagnostic instead of a parse error about some unrelated field.
//!
//! **Forward compatibility is explicit.** Unknown fields are ignored and unknown
//! enum values deserialise to `Unknown` variants. Servers must be able to add
//! capabilities without stranding devices that will never be updated again if they
//! choke on them. [`Source::Chunked`] exists in the schema from v1 precisely so that
//! delta transport can be introduced later with no `manifest_version` bump: a device
//! that does not implement it simply skips it and takes the full image.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::version::{MANIFEST_VERSION, OsVersion};

/// Reads `manifest_version` and nothing else.
///
/// Every other field is ignored, so this parses successfully against a manifest from
/// any future release. That is the point: a device must be able to say *which* version
/// it could not handle.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct VersionProbe {
    /// Structure version of the manifest.
    pub manifest_version: u32,
}

/// A manifest as fetched, with the exact bytes the detached signature covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawManifest {
    bytes: Vec<u8>,
}

impl RawManifest {
    /// Wraps fetched bytes. No parsing happens here.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// The exact bytes the Ed25519 detached signature is computed over.
    ///
    /// Verify against these and nothing else.
    #[must_use]
    pub fn signed_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Reads only `manifest_version`, tolerating any structure around it.
    ///
    /// # Errors
    /// Fails if the bytes are not JSON or carry no `manifest_version`.
    pub fn probe_version(&self) -> Result<u32, ManifestError> {
        serde_json::from_slice::<VersionProbe>(&self.bytes)
            .map(|p| p.manifest_version)
            .map_err(|e| ManifestError::Malformed(e.to_string()))
    }

    /// Parses the manifest, refusing an unsupported `manifest_version` first.
    ///
    /// This does **not** check the signature. Callers must verify
    /// [`RawManifest::signed_bytes`] before trusting anything returned here.
    ///
    /// # Errors
    /// Returns [`ManifestError::UnsupportedVersion`] for a version this build does not
    /// implement, or [`ManifestError::Malformed`] if the body does not parse.
    pub fn parse(&self) -> Result<Manifest, ManifestError> {
        let found = self.probe_version()?;
        if found != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion {
                found,
                supported: MANIFEST_VERSION,
            });
        }
        serde_json::from_slice(&self.bytes).map_err(|e| ManifestError::Malformed(e.to_string()))
    }
}

/// Why a manifest could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The manifest declares a structure version this build does not implement.
    UnsupportedVersion {
        /// Version declared by the manifest.
        found: u32,
        /// Version this build implements.
        supported: u32,
    },
    /// The bytes are not a well-formed manifest.
    Malformed(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "manifest version {found} is not supported by this release \
                 (supports {supported}); update to a newer PlexOS release first"
            ),
            Self::Malformed(detail) => write!(f, "malformed manifest: {detail}"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// A parsed, not-yet-trusted update manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Structure version. Always the first field, in the file and here.
    pub manifest_version: u32,
    /// Product identifier; guards against manifests from a different product line.
    pub product: String,
    /// Release channel this manifest was published to.
    pub channel: Channel,
    /// Human-readable release version. Never used to decide update eligibility.
    pub os_version: OsVersion,
    /// Monotonic anti-rollback counter.
    ///
    /// This, not [`Manifest::os_version`], is the security boundary. A device persists
    /// the highest sequence it has accepted and refuses anything lower, so an attacker
    /// replaying an old but validly signed manifest cannot downgrade it into a release
    /// with a known vulnerability.
    pub sequence: u64,
    /// RFC 3339 publication timestamp, for diagnostics only.
    pub created_at: String,
    /// Lowest already-installed sequence this update may be applied on top of.
    ///
    /// Set when an intermediate release performs a migration that cannot be skipped.
    #[serde(default)]
    pub min_sequence: Option<u64>,
    /// The `/usr` image and its verity data.
    pub usr: UsrPayload,
    /// The Unified Kernel Image, carrying the verity root hash on its command line.
    pub uki: Artifact,
    /// Which key signed this manifest.
    pub signing: Signing,
}

/// Release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Production releases.
    Stable,
    /// Pre-release testing.
    Beta,
    /// Development builds.
    Dev,
    /// A channel introduced after this build. Never matches a configured channel.
    #[serde(other)]
    Unknown,
}

/// The `/usr` image payload and the verity data that authenticates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsrPayload {
    /// Filesystem format of the image.
    pub format: ImageFormat,
    /// The image itself.
    pub image: Artifact,
    /// The dm-verity hash tree for the image.
    pub verity: Verity,
}

/// Read-only filesystem format of a `/usr` image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    /// EROFS, the format used from v1.
    Erofs,
    /// A format introduced after this build.
    #[serde(other)]
    Unknown,
}

/// dm-verity data for a `/usr` image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verity {
    /// Root hash of the Merkle tree, lowercase hex.
    ///
    /// Must equal the root hash embedded in the UKI command line. The updater checks
    /// this: a mismatch means the pair would produce an unbootable slot.
    pub root_hash: String,
    /// The hash tree written to the slot's verity partition.
    pub hashes: Artifact,
}

/// A downloadable artifact, addressed by content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Size in bytes of the reconstructed artifact.
    pub size: u64,
    /// SHA-256 of the reconstructed artifact, lowercase hex.
    ///
    /// Every transport must produce bytes matching this, so a delta source can never
    /// be trusted more loosely than a full download.
    pub sha256: String,
    /// Ways to obtain the artifact, in the publisher's order of preference.
    pub sources: Vec<Source>,
}

impl Artifact {
    /// The first source this build knows how to fetch, if any.
    ///
    /// A manifest whose sources are all unsupported is not an error in itself — it is
    /// an update this device cannot take yet, which is a reportable condition rather
    /// than a failure.
    #[must_use]
    pub fn first_supported_source(&self) -> Option<&Source> {
        self.sources.iter().find(|s| s.is_supported())
    }
}

/// A way to obtain an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Download the complete artifact.
    Full {
        /// Location of the artifact.
        url: String,
    },
    /// Reconstruct the artifact from content-addressed chunks, reusing what the
    /// device already has.
    ///
    /// Present in the schema since v1 but not implemented by this build. It exists so
    /// that delta transport can be enabled server-side later without a
    /// `manifest_version` bump — devices that do not implement it fall through to a
    /// full source.
    Chunked {
        /// Chunk index describing the artifact.
        index_url: String,
        /// Store the chunks are fetched from.
        store_url: String,
        /// Chunking algorithm identifier.
        algorithm: String,
    },
    /// A transport introduced after this build. Always skipped.
    #[serde(other)]
    Unknown,
}

impl Source {
    /// Whether this build can fetch from this source.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::Full { .. })
    }
}

/// Identifies the key that signed the manifest (ADR-0006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signing {
    /// Identifier of the signing key, for diagnostics and revocation lookups.
    pub key_id: String,
    /// The signing key's certificate, signed by an offline root key, base64.
    ///
    /// Travelling with the manifest is what makes rotation possible: a device needs no
    /// prior knowledge of the current signing key, only of the root keys baked into
    /// its `/usr` image and therefore covered by the UKI signature.
    pub certificate: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> RawManifest {
        RawManifest::new(
            include_bytes!("../tests/fixtures/manifest-v1.json")
                .as_slice()
                .to_vec(),
        )
    }

    #[test]
    fn parses_the_v1_fixture() {
        let m = fixture().parse().unwrap();
        assert_eq!(m.manifest_version, 1);
        assert_eq!(m.product, "plexos");
        assert_eq!(m.channel, Channel::Stable);
        assert_eq!(m.os_version, OsVersion::new(0, 1, 0));
        assert_eq!(m.sequence, 1);
        assert_eq!(m.usr.format, ImageFormat::Erofs);
    }

    #[test]
    fn signed_bytes_are_returned_verbatim() {
        let raw = fixture();
        let original = include_bytes!("../tests/fixtures/manifest-v1.json").as_slice();
        assert_eq!(raw.signed_bytes(), original);
    }

    #[test]
    fn structure_survives_a_serialise_parse_round_trip() {
        let parsed = fixture().parse().unwrap();
        let reencoded = RawManifest::new(serde_json::to_vec(&parsed).unwrap());
        assert_eq!(reencoded.parse().unwrap(), parsed);
    }

    #[test]
    fn refuses_a_future_manifest_version_by_name() {
        let raw = RawManifest::new(br#"{"manifest_version": 99}"#.to_vec());
        assert_eq!(raw.probe_version().unwrap(), 99);
        assert_eq!(
            raw.parse().unwrap_err(),
            ManifestError::UnsupportedVersion {
                found: 99,
                supported: MANIFEST_VERSION,
            }
        );
    }

    #[test]
    fn probes_the_version_of_an_otherwise_unparseable_manifest() {
        // A v2 manifest whose body this build cannot make sense of must still report
        // its version, or the device can say nothing useful about why it is stuck.
        let raw = RawManifest::new(
            br#"{"manifest_version": 2, "payloads": [{"totally": "different"}]}"#.to_vec(),
        );
        assert_eq!(raw.probe_version().unwrap(), 2);
    }

    #[test]
    fn ignores_fields_added_by_a_later_publisher() {
        let mut json: serde_json::Value = serde_json::from_slice(fixture().signed_bytes()).unwrap();
        json["field_from_the_future"] = serde_json::json!({"nested": [1, 2, 3]});
        let raw = RawManifest::new(serde_json::to_vec(&json).unwrap());
        assert_eq!(raw.parse().unwrap().sequence, 1);
    }

    #[test]
    fn skips_unknown_source_kinds_and_falls_through_to_a_full_download() {
        let sources: Vec<Source> = serde_json::from_str(
            r#"[
                {"kind": "bittorrent", "magnet": "..."},
                {"kind": "chunked", "index_url": "i", "store_url": "s", "algorithm": "x"},
                {"kind": "full", "url": "https://example.invalid/usr.erofs"}
            ]"#,
        )
        .unwrap();
        assert_eq!(sources[0], Source::Unknown);
        assert!(!sources[1].is_supported(), "chunked is schema-only in v1");

        let artifact = Artifact {
            size: 1,
            sha256: String::new(),
            sources,
        };
        assert!(matches!(
            artifact.first_supported_source(),
            Some(Source::Full { .. })
        ));
    }

    #[test]
    fn reports_no_supported_source_rather_than_guessing() {
        let artifact = Artifact {
            size: 1,
            sha256: String::new(),
            sources: vec![Source::Chunked {
                index_url: "i".into(),
                store_url: "s".into(),
                algorithm: "x".into(),
            }],
        };
        assert!(artifact.first_supported_source().is_none());
    }

    #[test]
    fn an_unknown_channel_never_matches_a_configured_one() {
        let channel: Channel = serde_json::from_str(r#""lts""#).unwrap();
        assert_eq!(channel, Channel::Unknown);
        for known in [Channel::Stable, Channel::Beta, Channel::Dev] {
            assert_ne!(channel, known);
        }
    }
}
