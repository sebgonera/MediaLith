//! Version types.
//!
//! MediaLith carries four independent version numbers, and conflating any two of them is
//! a bug:
//!
//! | Version | Meaning | Where it lives |
//! | --- | --- | --- |
//! | [`OsVersion`] | Human-readable release of the OS image | Manifest, UKI filename |
//! | [`MANIFEST_VERSION`] | Structure of the update manifest | Manifest, first field |
//! | [`CONFIG_SCHEMA_VERSION`] | Structure of `config.toml` | Config, first field |
//! | [`STATE_LAYOUT_VERSION`] | Structure of `/var` | `/var/lib/plexos/STATE_VERSION` |
//!
//! A fifth number, the update manifest's `sequence`, is *not* a version — it is the
//! anti-rollback counter, and it is the only one with security meaning. See
//! [`crate::manifest`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// What this product is called in machine-readable places.
///
/// The manifest's `product` field and `os-release`'s `ID` are the same string, and an
/// update whose product does not match is one built for a different appliance — a check
/// that costs nothing and that a signature cannot make for you, since a correctly signed
/// manifest for something else is still correctly signed.
///
/// Lower-case and no spaces, because `os-release` says so.
///
/// # Legacy internal identifier, retained deliberately
///
/// The product is **MediaLith**. This still says `plexos`, and changing it would not be
/// a rename — it would make the rename undeliverable.
///
/// `plan::for_manifest` refuses any bundle whose `product` differs from this constant,
/// with "this update is for X and this appliance is Y". Every appliance already in the
/// world runs a build compiled with `plexos` here. Publishing a MediaLith bundle that
/// said `medialith` would therefore be **refused by every existing machine**, and the
/// only way to install it would be a reinstall — which gives the machine a fresh `/var`,
/// and with it a new device token, a new TLS identity and an unprovisioned Plex.
///
/// The check is doing its job in that scenario. The mistake would be ours: this string
/// identifies the *product line whose updates are interchangeable*, and a rebrand does not
/// change which images are interchangeable. It can only move when nothing in the field
/// still expects the old value, which is a migration to design on its own.
pub const PRODUCT: &str = "plexos";

/// Manifest structure version understood by this build (ADR-0006).
pub const MANIFEST_VERSION: u32 = 1;

#[cfg(test)]
mod product_identity {
    /// The product was renamed to MediaLith and this constant was not, deliberately.
    ///
    /// A test rather than a comment because the comment is what a future rename would
    /// delete. Changing this string makes every appliance in the field refuse every
    /// update built after the change — the failure is total, silent until somebody tries
    /// to update, and unfixable except by reinstalling each machine.
    ///
    /// If this ever has to move, it moves as a designed migration in which a release
    /// accepts *both* values for long enough that no machine is left behind. Until such a
    /// release has been in the field, this test failing means the change is wrong.
    #[test]
    fn the_update_product_identifier_is_still_the_legacy_one() {
        assert_eq!(
            super::PRODUCT,
            "plexos",
            "renaming this refuses every update on every machine already installed; \
             see the constant's documentation before changing this test"
        );
    }
}

/// Configuration schema version understood by this build (ADR-0008).
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Layout version of `/var` expected by this build (ADR-0009).
pub const STATE_LAYOUT_VERSION: u32 = 1;

/// A MediaLith release version: `MAJOR.MINOR.PATCH`.
///
/// Ordering is numeric per component, so `0.10.0` correctly sorts after `0.9.0`.
/// This ordering is for display and diagnostics only — update eligibility is decided
/// by the manifest sequence counter, never by comparing these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OsVersion {
    /// Major component.
    pub major: u32,
    /// Minor component.
    pub minor: u32,
    /// Patch component.
    pub patch: u32,
}

impl OsVersion {
    /// Constructs a version from its components.
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

/// Reason a version string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseVersionError {
    /// The string did not have exactly three dot-separated components.
    Shape,
    /// A component was empty, non-numeric, or out of range.
    Component,
}

impl fmt::Display for ParseVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape => f.write_str("expected MAJOR.MINOR.PATCH"),
            Self::Component => f.write_str("version components must be integers"),
        }
    }
}

impl std::error::Error for ParseVersionError {}

impl FromStr for OsVersion {
    type Err = ParseVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('.');
        let mut next = || -> Result<u32, ParseVersionError> {
            parts
                .next()
                .ok_or(ParseVersionError::Shape)?
                .parse()
                .map_err(|_| ParseVersionError::Component)
        };
        let major = next()?;
        let minor = next()?;
        let patch = next()?;
        if parts.next().is_some() {
            return Err(ParseVersionError::Shape);
        }
        Ok(Self::new(major, minor, patch))
    }
}

impl fmt::Display for OsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for OsVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OsVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_round_trip() {
        let v: OsVersion = "1.2.3".parse().unwrap();
        assert_eq!(v, OsVersion::new(1, 2, 3));
        assert_eq!(v.to_string(), "1.2.3");
    }

    #[test]
    fn orders_numerically_not_lexically() {
        let a: OsVersion = "0.9.0".parse().unwrap();
        let b: OsVersion = "0.10.0".parse().unwrap();
        assert!(a < b, "0.10.0 must sort after 0.9.0");
    }

    #[test]
    fn rejects_malformed_versions() {
        for bad in ["1.2", "1.2.3.4", "1.2.x", "", "v1.2.3", "1.2.-3"] {
            assert!(bad.parse::<OsVersion>().is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn serialises_as_a_plain_string() {
        let json = serde_json::to_string(&OsVersion::new(0, 2, 1)).unwrap();
        assert_eq!(json, "\"0.2.1\"");
        assert_eq!(
            serde_json::from_str::<OsVersion>(&json).unwrap(),
            OsVersion::new(0, 2, 1)
        );
    }
}
