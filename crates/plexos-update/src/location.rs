//! Turning what a manifest says about where an artefact lives into something to fetch.
//!
//! # Why a name and not a URL
//!
//! The signature covers the manifest's exact bytes, so anything inside it is fixed at
//! signing time — including, if they were absolute, the addresses the artefacts are
//! fetched from. Every publish this project has done served a bundle from a different
//! place: a laptop's `python3 -m http.server`, on whatever address the build host had that
//! day. Absolute URLs would mean re-signing a bundle to move it, with a key that is
//! supposed to be offline.
//!
//! So a source may be a bare file name, resolved against wherever the manifest itself came
//! from. The publisher signs *what* to fetch; the person doing the publishing chooses
//! *from where*, and cannot alter the former by changing the latter.
//!
//! # Why the shape is refused rather than repaired
//!
//! The name is joined to a URL and, separately, to a staging path. A name containing `..`
//! or a leading `/` would let whoever wrote the manifest choose what this appliance reads
//! and where it writes. Sanitising is where the interesting bugs live, so a name that is
//! not a plain file name is refused outright — the same rule the improvised bundle format
//! used, kept because it was right.
//!
//! Staged files are named by *role* rather than by anything the publisher chose, which
//! makes the path side of that question moot: see [`Role`].

use plexos_types::manifest::{Artifact, Source};

/// What a downloaded artefact is for, and what it is called while it is on disk.
///
/// The staging name comes from here rather than from the manifest. A publisher choosing
/// file names on the appliance's disk is a decision nobody needs to make, and this is one
/// of two places a hostile name could have mattered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The `/usr` filesystem image.
    Usr,
    /// Its dm-verity hash tree.
    Verity,
    /// The Unified Kernel Image for the slot being written.
    Uki,
}

impl Role {
    /// The name this artefact is staged under.
    #[must_use]
    pub const fn staging_name(self) -> &'static str {
        match self {
            Self::Usr => "usr.erofs",
            Self::Verity => "usr.hash",
            Self::Uki => "uki.efi",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Usr => "the /usr image",
            Self::Verity => "the verity hash tree",
            Self::Uki => "the boot entry",
        })
    }
}

/// Why an artefact could not be turned into something to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationError {
    /// Every source this artefact declares is one this build cannot fetch from.
    NoSupportedSource {
        /// Which artefact.
        role: Role,
        /// The `kind` values that were offered.
        offered: Vec<String>,
    },
    /// The source is neither an absolute `http`/`https` URL nor a plain file name.
    UnsafeName {
        /// Which artefact.
        role: Role,
        /// What the manifest asked for.
        url: String,
    },
}

impl std::fmt::Display for LocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSupportedSource { role, offered } => write!(
                f,
                "this update offers {role} only as {}, and this release can fetch only a \
                 whole file. Remedy: this is not a broken bundle, it is one published for \
                 a newer appliance -- update this machine through a bundle that carries a \
                 full source, or publish one.",
                if offered.is_empty() {
                    "nothing at all".to_owned()
                } else {
                    offered.join(", ")
                }
            ),
            Self::UnsafeName { role, url } => write!(
                f,
                "this update locates {role} at {url}, which is neither an http(s) address \
                 nor a plain file name beside the manifest. Remedy: treat this bundle as \
                 hostile. A name with a path in it is an attempt to choose what this \
                 appliance reads and writes, and the publisher's own tooling cannot \
                 produce one."
            ),
        }
    }
}

impl std::error::Error for LocationError {}

/// Where to fetch `artifact` from, given where the manifest was fetched from.
///
/// # Errors
/// [`LocationError`], naming the artefact by role rather than by file name — the file name
/// is exactly the thing that may be untrustworthy here.
pub fn resolve(base: &str, role: Role, artifact: &Artifact) -> Result<String, LocationError> {
    let Some(Source::Full { url }) = artifact.first_supported_source() else {
        return Err(LocationError::NoSupportedSource {
            role,
            offered: artifact.sources.iter().map(kind_of).collect(),
        });
    };

    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(url.clone());
    }

    if !is_plain_name(url) {
        return Err(LocationError::UnsafeName {
            role,
            url: url.clone(),
        });
    }

    Ok(format!("{}/{url}", base.trim_end_matches('/')))
}

/// Whether a source is a plain file name and not an attempt to escape the bundle.
///
/// A leading dot is refused along with the rest: it buys nothing here, and it keeps the
/// answer to "is this a name I would be willing to write into a directory" the same in
/// both places that ask.
#[must_use]
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.starts_with('.')
        && !name.contains(':')
        && !name.contains('?')
        && !name.contains('#')
}

/// The `kind` of a source, for reporting what was offered.
fn kind_of(source: &Source) -> String {
    match source {
        Source::Full { .. } => "full".to_owned(),
        Source::Chunked { algorithm, .. } => format!("chunked/{algorithm}"),
        Source::Unknown => "a transport this release does not know".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(sources: Vec<Source>) -> Artifact {
        Artifact {
            size: 1,
            sha256: String::new(),
            sources,
        }
    }

    fn full(url: &str) -> Artifact {
        artifact(vec![Source::Full {
            url: url.to_owned(),
        }])
    }

    #[test]
    fn a_bare_name_is_resolved_against_wherever_the_manifest_came_from() {
        // The property that lets one signed bundle be served from anywhere.
        assert_eq!(
            resolve("http://192.168.2.9:8080/b", Role::Usr, &full("usr.erofs")).unwrap(),
            "http://192.168.2.9:8080/b/usr.erofs"
        );
        assert_eq!(
            resolve("http://192.168.2.9:8080/b/", Role::Usr, &full("usr.erofs")).unwrap(),
            "http://192.168.2.9:8080/b/usr.erofs",
            "a trailing slash on the source must not double up"
        );
    }

    #[test]
    fn an_absolute_address_is_taken_as_it_is() {
        assert_eq!(
            resolve(
                "http://elsewhere",
                Role::Uki,
                &full("https://cdn.example/x.efi")
            )
            .unwrap(),
            "https://cdn.example/x.efi"
        );
    }

    #[test]
    fn a_name_that_could_leave_the_bundle_is_refused_rather_than_repaired() {
        // Both halves of what a name is joined to: a URL and, elsewhere, a path. Every one
        // of these is a publisher choosing what this appliance reads.
        for bad in [
            "../../etc/shadow",
            "/etc/shadow",
            ".hidden",
            "a/b",
            "a\\b",
            "",
            "file:///etc/shadow",
            "http:/almost",
        ] {
            let error = resolve("http://host", Role::Usr, &full(bad)).unwrap_err();
            assert!(
                matches!(error, LocationError::UnsafeName { .. }),
                "{bad} was accepted"
            );
            assert!(error.to_string().contains("Remedy:"), "{bad}");
        }
    }

    #[test]
    fn an_update_this_release_cannot_fetch_is_reported_as_that_and_not_as_damage() {
        // A chunked-only bundle is a bundle published for a newer appliance, which is a
        // condition to report rather than a fault to investigate.
        let error = resolve(
            "http://host",
            Role::Verity,
            &artifact(vec![Source::Chunked {
                index_url: "i".to_owned(),
                store_url: "s".to_owned(),
                algorithm: "casync".to_owned(),
            }]),
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("chunked/casync"), "{message}");
        assert!(message.contains("verity hash tree"), "{message}");
        assert!(message.contains("not a broken bundle"), "{message}");
    }

    #[test]
    fn an_unknown_transport_is_skipped_in_favour_of_one_that_works() {
        let mixed = artifact(vec![
            Source::Unknown,
            Source::Full {
                url: "usr.erofs".to_owned(),
            },
        ]);
        assert_eq!(
            resolve("http://host", Role::Usr, &mixed).unwrap(),
            "http://host/usr.erofs"
        );
    }

    #[test]
    fn staging_names_come_from_the_role_and_never_from_the_manifest() {
        // The other place a hostile name would have mattered. Nothing a publisher writes
        // reaches a path on this appliance.
        assert_eq!(Role::Usr.staging_name(), "usr.erofs");
        assert_eq!(Role::Verity.staging_name(), "usr.hash");
        assert_eq!(Role::Uki.staging_name(), "uki.efi");
        for role in [Role::Usr, Role::Verity, Role::Uki] {
            assert!(is_plain_name(role.staging_name()));
        }
    }
}
