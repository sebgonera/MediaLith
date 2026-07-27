//! Provisioning Plex Media Server from Plex's own signed packages (ADR-0010).
//!
//! PlexOS images never contain Plex: it is proprietary and not ours to redistribute, so
//! the appliance fetches it at first boot and converts it into an app image (ADR-0007).
//! Everything that happens to that download before it is trusted lives here.
//!
//! # The order of operations is the security property
//!
//! 1. [`ar::directory`] locates the members without reading their payloads.
//! 2. `gpgv` verifies the `_gpgplex` clearsigned manifest against a **pinned** copy of
//!    Plex's key — pinned in the image, never fetched alongside the artefact it is
//!    supposed to vouch for.
//! 3. [`manifest::parse`] reads the verified text.
//! 4. [`agrees_with`] checks the package on disk against that text.
//! 5. Only then is `data.tar.xz` unpacked.
//!
//! Step 4 is the one that is easy to leave out and fatal to leave out. A verified
//! signature over a manifest that was never compared to the payload proves that Plex
//! signed *something*, and nothing whatever about the bytes on this disk.
//!
//! # What is verified and what is not
//!
//! The signature chain has been checked end to end against both of Plex's channels — the
//! APT repository and the direct download — on 2026-07-27, using the key published at
//! `downloads.plex.tv/plex-keys/PlexSign.key`, fingerprint
//! `CD665CBA0E2F88B7373F7CB997203C7B3ADCA79D`. `GnuPG` 2.4.8 accepts both without
//! `--allow-weak-digest-algos`, which is worth knowing because the manifest's digests
//! are MD5 and SHA1: a stricter `GnuPG` would break provisioning on the device while
//! still working on a build host.
//!
//! **Nothing in this crate has run on the appliance.** Delete this notice when it has.

#![forbid(unsafe_code)]

pub mod ar;
pub mod build;
pub mod execute;
pub mod manifest;
pub mod mount;
pub mod store;
pub mod tools;
pub mod verify;

/// Fingerprint of the key every Plex package and repository index is signed with.
///
/// Pinned as a constant so that trust comes from the image rather than from whatever
/// key happens to arrive with a download. Rotating it is a deliberate release change,
/// which is the point.
pub const PLEX_KEY_FINGERPRINT: &str = "CD665CBA0E2F88B7373F7CB997203C7B3ADCA79D";

/// The member holding the clearsigned manifest.
pub const SIGNATURE_MEMBER: &str = "_gpgplex";

/// Members that carry no payload of ours and are not expected in the manifest.
///
/// The signature cannot cover itself.
const NOT_SIGNED: [&str; 1] = [SIGNATURE_MEMBER];

/// A member as found on disk, with the digest computed from its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measured {
    /// Member name, from the `ar` directory.
    pub name: String,
    /// Size in bytes, from the `ar` directory.
    pub size: u64,
    /// SHA1 of the payload, lower-case hex.
    pub sha1: String,
}

/// Why a package does not match the manifest Plex signed for it.
#[derive(Debug, PartialEq, Eq)]
pub enum Mismatch {
    /// A member is in the package but not in the manifest.
    Unlisted(String),
    /// A member is in the manifest but not in the package.
    Missing(String),
    /// A member's length differs.
    Size {
        /// Which member.
        name: String,
        /// What the signed manifest says.
        signed: u64,
        /// What is on disk.
        found: u64,
    },
    /// A member's SHA1 differs.
    Digest {
        /// Which member.
        name: String,
        /// What the signed manifest says.
        signed: String,
        /// What is on disk.
        found: String,
    },
    /// The package has no signature member.
    Unsigned,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unlisted(name) => write!(
                f,
                "the package contains {name}, which the signed manifest does not list. \
                 Something was added after Plex signed it; do not install this package."
            ),
            Self::Missing(name) => write!(
                f,
                "the signed manifest lists {name}, which the package does not contain. \
                 The download is incomplete or has been altered; fetch it again."
            ),
            Self::Size {
                name,
                signed,
                found,
            } => write!(
                f,
                "{name} is {found} bytes; Plex signed for {signed}. Do not install this \
                 package."
            ),
            Self::Digest {
                name,
                signed,
                found,
            } => write!(
                f,
                "{name} hashes to {found}; Plex signed for {signed}. Do not install this \
                 package."
            ),
            Self::Unsigned => write!(
                f,
                "the package has no {SIGNATURE_MEMBER} member, so there is nothing to \
                 verify it against. Every package from Plex's own channels carries one — \
                 this did not come from there, or it was rebuilt on the way."
            ),
        }
    }
}

/// Does the package on disk match the manifest Plex signed?
///
/// `measured` comes from the `ar` directory with each payload hashed; `signed` is the
/// parsed body of an *already verified* `_gpgplex`. Returns every discrepancy rather
/// than the first, because "this package differs in four ways" and "this package has a
/// truncated data member" call for the same decision but very different investigations.
///
/// Both directions are checked. Confirming only that each signed entry is present would
/// accept a package with an extra member bolted on after signing, which is precisely
/// how a payload gets smuggled past a manifest that is otherwise entirely genuine.
#[must_use]
pub fn agrees_with(measured: &[Measured], signed: &manifest::Manifest) -> Vec<Mismatch> {
    let mut problems = Vec::new();

    if !measured.iter().any(|m| m.name == SIGNATURE_MEMBER) {
        problems.push(Mismatch::Unsigned);
    }

    for found in measured
        .iter()
        .filter(|m| !NOT_SIGNED.contains(&m.name.as_str()))
    {
        let Some(entry) = signed.entries.iter().find(|e| e.name == found.name) else {
            problems.push(Mismatch::Unlisted(found.name.clone()));
            continue;
        };
        if entry.size != found.size {
            problems.push(Mismatch::Size {
                name: found.name.clone(),
                signed: entry.size,
                found: found.size,
            });
        }
        if !entry.sha1.eq_ignore_ascii_case(&found.sha1) {
            problems.push(Mismatch::Digest {
                name: found.name.clone(),
                signed: entry.sha1.clone(),
                found: found.sha1.clone(),
            });
        }
    }

    for entry in &signed.entries {
        if !measured.iter().any(|m| m.name == entry.name) {
            problems.push(Mismatch::Missing(entry.name.clone()));
        }
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_manifest() -> manifest::Manifest {
        manifest::Manifest {
            signer: "Plex Inc.".to_owned(),
            role: "plex".to_owned(),
            entries: vec![
                manifest::Entry {
                    md5: "3cf918272ffa5de195752d73f3da3e5e".to_owned(),
                    sha1: "7959c969e092f2a5a8604e2287807ac5b1b384ad".to_owned(),
                    size: 4,
                    name: "debian-binary".to_owned(),
                },
                manifest::Entry {
                    md5: "36cbc776f8dc78c76146a8b3d54fb000".to_owned(),
                    sha1: "8b2fc4685435bd956d6e9b507d0e3bfbe503dce3".to_owned(),
                    size: 83_047_296,
                    name: "data.tar.xz".to_owned(),
                },
            ],
        }
    }

    fn measured(overrides: &[(&str, u64, &str)]) -> Vec<Measured> {
        let mut out: Vec<Measured> = overrides
            .iter()
            .map(|(name, size, sha1)| Measured {
                name: (*name).to_owned(),
                size: *size,
                sha1: (*sha1).to_owned(),
            })
            .collect();
        out.push(Measured {
            name: SIGNATURE_MEMBER.to_owned(),
            size: 1242,
            sha1: "0".repeat(40),
        });
        out
    }

    #[test]
    fn a_package_matching_its_manifest_has_nothing_to_report() {
        let found = measured(&[
            (
                "debian-binary",
                4,
                "7959c969e092f2a5a8604e2287807ac5b1b384ad",
            ),
            (
                "data.tar.xz",
                83_047_296,
                "8b2fc4685435bd956d6e9b507d0e3bfbe503dce3",
            ),
        ]);
        assert_eq!(agrees_with(&found, &signed_manifest()), []);
    }

    #[test]
    fn a_member_added_after_signing_is_caught() {
        // The attack the reverse direction exists for. Every signed entry is present
        // and correct; a fourth member has simply been appended. Checking only that the
        // manifest's entries match would pass this.
        let mut found = measured(&[
            (
                "debian-binary",
                4,
                "7959c969e092f2a5a8604e2287807ac5b1b384ad",
            ),
            (
                "data.tar.xz",
                83_047_296,
                "8b2fc4685435bd956d6e9b507d0e3bfbe503dce3",
            ),
        ]);
        found.push(Measured {
            name: "postinst.tar.xz".to_owned(),
            size: 512,
            sha1: "a".repeat(40),
        });

        let problems = agrees_with(&found, &signed_manifest());
        assert_eq!(problems, [Mismatch::Unlisted("postinst.tar.xz".to_owned())]);
        assert!(
            problems[0].to_string().contains("do not install"),
            "names the decision: {}",
            problems[0]
        );
    }

    #[test]
    fn an_unsigned_package_is_refused_even_when_nothing_else_is_wrong() {
        // A .deb built from the same sources, with the signature member dropped. Every
        // remaining comparison would be vacuous, since there is no manifest to compare
        // against -- so the absence itself has to be the finding.
        let found = vec![Measured {
            name: "debian-binary".to_owned(),
            size: 4,
            sha1: "7959c969e092f2a5a8604e2287807ac5b1b384ad".to_owned(),
        }];
        let problems = agrees_with(&found, &signed_manifest());
        assert!(problems.contains(&Mismatch::Unsigned), "{problems:?}");
    }

    #[test]
    fn a_truncated_payload_is_reported_by_size_and_by_digest() {
        let found = measured(&[
            (
                "debian-binary",
                4,
                "7959c969e092f2a5a8604e2287807ac5b1b384ad",
            ),
            ("data.tar.xz", 40_000_000, &"b".repeat(40)),
        ]);
        let problems = agrees_with(&found, &signed_manifest());
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| matches!(p, Mismatch::Size { .. })));
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, Mismatch::Digest { .. }))
        );
    }

    #[test]
    fn a_missing_member_is_reported_from_the_manifests_side() {
        let found = measured(&[(
            "debian-binary",
            4,
            "7959c969e092f2a5a8604e2287807ac5b1b384ad",
        )]);
        let problems = agrees_with(&found, &signed_manifest());
        assert!(problems.contains(&Mismatch::Missing("data.tar.xz".to_owned())));
    }

    #[test]
    fn the_signature_member_is_not_expected_to_sign_itself() {
        // _gpgplex is present in every package and in no manifest. Treating it like any
        // other member would report a permanent, spurious Unlisted on every genuine
        // package -- a false alarm that trains whoever reads these to ignore them.
        let found = measured(&[
            (
                "debian-binary",
                4,
                "7959c969e092f2a5a8604e2287807ac5b1b384ad",
            ),
            (
                "data.tar.xz",
                83_047_296,
                "8b2fc4685435bd956d6e9b507d0e3bfbe503dce3",
            ),
        ]);
        assert!(found.iter().any(|m| m.name == SIGNATURE_MEMBER));
        assert_eq!(agrees_with(&found, &signed_manifest()), []);
    }

    #[test]
    fn digests_compare_without_regard_to_case() {
        let found = measured(&[
            (
                "debian-binary",
                4,
                "7959C969E092F2A5A8604E2287807AC5B1B384AD",
            ),
            (
                "data.tar.xz",
                83_047_296,
                "8b2fc4685435bd956d6e9b507d0e3bfbe503dce3",
            ),
        ]);
        assert_eq!(agrees_with(&found, &signed_manifest()), []);
    }

    #[test]
    fn the_pinned_fingerprint_is_the_key_plex_publishes() {
        // Captured from downloads.plex.tv/plex-keys/PlexSign.key on 2026-07-27 and
        // confirmed against the signature on both channels. If this fails, the constant
        // is what changes -- and only after checking the new key is genuinely Plex's.
        assert_eq!(
            PLEX_KEY_FINGERPRINT,
            "CD665CBA0E2F88B7373F7CB997203C7B3ADCA79D"
        );
        assert_eq!(PLEX_KEY_FINGERPRINT.len(), 40, "a full v4 fingerprint");
    }
}
