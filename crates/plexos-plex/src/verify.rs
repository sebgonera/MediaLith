//! Checking Plex's signature, and refusing to guess when it cannot be checked.
//!
//! `gpgv` does the cryptography. Reimplementing `OpenPGP` verification here would mean
//! writing the one kind of code where a subtle bug is silent and catastrophic, to avoid
//! a 566 KB dependency the image already carries for this purpose.
//!
//! # Why the keyring is a path and not a download
//!
//! [`PLEX_KEYRING`] lives in `/usr`, which is read-only and verified by dm-verity
//! (ADR-0004). A key fetched alongside the artefact it vouches for verifies nothing at
//! all — whoever supplied the artefact supplied the key. Pinning it in the image makes
//! replacing the trust root as hard as replacing a signed OS image, which is the
//! ceremony it deserves.
//!
//! # `gpgv` is not `gpg`, and the difference is the point
//!
//! `gpgv` has no keyring management, no web of trust, no configuration files, and no
//! way to be talked into importing a key. It verifies against exactly the keyring it is
//! given and exits non-zero otherwise. That is a smaller thing to reason about than
//! `gpg` with the right flags — and getting `gpg`'s flags subtly wrong is a well-worn
//! way to build something that reports success for a signature by any key at all.

use std::path::Path;
use std::process::Command;

/// Where the pinned key lives, installed by `package/plexos-plex-keyring`.
///
/// A test asserts this against the Buildroot recipe, because the two are edited in
/// different files by different people and a mismatch produces "cannot provision" on a
/// user's machine rather than anything at build time.
pub const PLEX_KEYRING: &str = "/usr/share/plexos/plex-signing-key.gpg";

/// Where `gpgv` is, resolved absolutely.
///
/// Not looked up on `PATH`, for the reason recorded in `plexosd::net`: provisioning may
/// run from a process started by PID 1, whose environment is empty, and glibc's
/// fallback path does not include the directory busybox and gnupg2 install into.
const GPGV_CANDIDATES: [&str; 3] = ["/usr/bin/gpgv", "/bin/gpgv", "/usr/local/bin/gpgv"];

/// Why a signature could not be accepted.
#[derive(Debug)]
pub enum Error {
    /// `gpgv` is not installed.
    NoVerifier,
    /// The pinned keyring is not where it should be.
    NoKeyring(String),
    /// `gpgv` ran and rejected the signature.
    Rejected {
        /// What `gpgv` said, for the report.
        output: String,
    },
    /// `gpgv` could not be run at all.
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoVerifier => write!(
                f,
                "gpgv is not installed, so the package's signature cannot be checked and \
                 will not be assumed good. This is an image fault: \
                 BR2_PACKAGE_GNUPG2_GPGV should have put it in {}.",
                GPGV_CANDIDATES[0]
            ),
            Self::NoKeyring(path) => write!(
                f,
                "the pinned Plex signing key is missing from {path}, so there is nothing \
                 to verify against. This is an image fault, not a problem with the \
                 download: package/plexos-plex-keyring installs it into /usr."
            ),
            Self::Rejected { output } => write!(
                f,
                "Plex's signature on this package was rejected. Do not install it. If it \
                 came from Plex's own servers the download is damaged, so fetch it \
                 again; if it came from anywhere else, it is not Plex's. gpgv said: \
                 {output}"
            ),
            Self::Io(error) => write!(f, "could not run gpgv: {error}"),
        }
    }
}

impl std::error::Error for Error {}

/// The first candidate path that exists.
fn find_gpgv(exists: &dyn Fn(&Path) -> bool) -> Option<&'static str> {
    GPGV_CANDIDATES
        .into_iter()
        .find(|candidate| exists(Path::new(candidate)))
}

/// Verifies a clearsigned document and returns the text the signature covers.
///
/// Returns the **verified plaintext**, not a boolean. A function that answered "yes,
/// that is signed" would leave the caller to read the document separately, and nothing
/// would stop it reading a different one — which is the gap that makes a signature
/// check decorative. Handing back the covered bytes closes it by construction.
///
/// # Errors
/// See [`Error`]. Every variant means the package must not be installed. There is no
/// "could not check, carrying on" path, and adding one would defeat the module.
pub fn clearsigned(document: &Path, keyring: &Path) -> Result<String, Error> {
    let exists = |p: &Path| p.exists();
    let Some(gpgv) = find_gpgv(&exists) else {
        return Err(Error::NoVerifier);
    };
    if !keyring.exists() {
        return Err(Error::NoKeyring(keyring.display().to_string()));
    }

    // --output - writes the covered plaintext to stdout, and gpgv writes it only when
    // the signature is good. Reading the file ourselves instead would mean the bytes we
    // parse and the bytes gpgv checked are two separate reads of a file that another
    // process could change in between.
    let result = Command::new(gpgv)
        .arg("--keyring")
        .arg(keyring)
        .arg("--output")
        .arg("-")
        .arg(document)
        .output()
        .map_err(Error::Io)?;

    if !result.status.success() {
        return Err(Error::Rejected {
            output: String::from_utf8_lossy(&result.stderr)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }

    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyring_path_matches_the_buildroot_recipe() {
        // These two are edited in different files. A mismatch is invisible until a user
        // tries to provision and is told the key is missing from a path that looks
        // perfectly plausible.
        let recipe =
            include_str!("../../../buildroot/package/plexos-plex-keyring/plexos-plex-keyring.mk");
        assert!(
            recipe.contains(&format!("PLEXOS_PLEX_KEYRING_TARGET = {PLEX_KEYRING}")),
            "the recipe must install the keyring where this module looks for it: \
             {PLEX_KEYRING}"
        );
    }

    #[test]
    fn the_recipe_pins_the_same_fingerprint_this_crate_does() {
        let recipe =
            include_str!("../../../buildroot/package/plexos-plex-keyring/plexos-plex-keyring.mk");
        assert!(
            recipe.contains(crate::PLEX_KEY_FINGERPRINT),
            "the build-time check and the crate must pin one key, not two"
        );
    }

    #[test]
    fn gpgv_is_looked_for_by_absolute_path() {
        // The PATH trap from plexosd::net, which cost a boot: this may run from a
        // process whose environment is empty, and gnupg2 installs into /usr/bin, which
        // glibc's fallback path does not cover.
        for candidate in GPGV_CANDIDATES {
            assert!(
                candidate.starts_with('/'),
                "{candidate} must be absolute, not resolved through PATH"
            );
        }
    }

    #[test]
    fn the_first_existing_candidate_wins() {
        let only_bin = |p: &Path| p == Path::new("/bin/gpgv");
        assert_eq!(find_gpgv(&only_bin), Some("/bin/gpgv"));

        let none = |_: &Path| false;
        assert_eq!(find_gpgv(&none), None);
    }

    #[test]
    fn a_missing_verifier_is_an_image_fault_and_says_so() {
        // The dangerous reading of "gpgv not found" is "skip the check". The message
        // has to make clear that the package is not thereby acceptable.
        let message = Error::NoVerifier.to_string();
        assert!(message.contains("will not be assumed good"), "{message}");
        assert!(message.contains("image fault"), "{message}");
    }

    #[test]
    fn a_rejected_signature_says_not_to_install_and_distinguishes_the_causes() {
        let message = Error::Rejected {
            output: "BAD signature".to_owned(),
        }
        .to_string();
        assert!(message.contains("Do not install"), "{message}");
        assert!(
            message.contains("fetch it again"),
            "damaged download is the common case: {message}"
        );
    }

    #[test]
    fn a_missing_keyring_is_not_reported_as_a_bad_download() {
        // Both leave the package unverified, and they need opposite responses: one is
        // ours to fix in the image, the other is the user's to re-download.
        let message = Error::NoKeyring(PLEX_KEYRING.to_owned()).to_string();
        assert!(message.contains("image fault"), "{message}");
        assert!(
            message.contains("not a problem with the download"),
            "{message}"
        );
    }
}
