//! The device token, and what it does and does not protect (ADR-0013).
//!
//! One appliance, one administrator, one secret. There is no user database, no password
//! and no default credential: the token is 256 bits from `/dev/urandom`, generated on
//! the machine at first start and shown on the console attached to it. Whoever can read
//! that screen may claim the device, which is the same trust model as a router's reset
//! button and is stated so that it is a decision rather than an oversight.
//!
//! # Why a token and not a password
//!
//! A key-derivation function exists to make guessing a low-entropy secret expensive.
//! Against 256 bits of entropy there is nothing to guess, so a single SHA-256 is
//! sufficient and no KDF — and therefore no hand-written cryptography — is needed. What
//! hashing does buy is that a copy of `/var`, from a backup or a pulled disk, yields
//! nothing that can be presented to the console.
//!
//! For the same reason there is no lockout and no rate limit. Both exist to slow
//! guessing; here they would only give anyone on the LAN a way to lock the
//! administrator out of their own appliance.
//!
//! # What this does not protect against
//!
//! The console is plain HTTP. A token crossing an unencrypted LAN is visible to anything
//! that can see the traffic. ADR-0013 accepts that for v1 and records it as the weakest
//! part of the design; the trigger for revisiting it is the console becoming reachable
//! from beyond a LAN, not the passage of time.
//!
//! Nor does a valid token say anything about *what* is uploaded. It permits installing a
//! package; ADR-0010's signature check is what decides whether that package is Plex.

use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Bytes of entropy in a token.
///
/// 32, so that guessing is not a threat model and the absence of a lockout needs no
/// further defence.
pub const TOKEN_BYTES: usize = 32;

/// Where the kernel's randomness comes from.
///
/// `/dev/urandom` rather than `/dev/random`: on Linux they draw from the same pool once
/// it is initialised, and the appliance generates this after the health gate has run —
/// seconds into userspace, long past initialisation. `/dev/random` would add the
/// possibility of blocking forever on a machine with no entropy sources for nothing.
const RANDOM_SOURCE: &str = "/dev/urandom";

/// A token as the administrator sees it: lower-case hex.
///
/// Hex rather than base64 because it survives being read off a screen and typed back in
/// without a case-sensitivity argument or a `+`/`/` mistaken for punctuation.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// The stored form of a token.
#[must_use]
pub fn fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    encode(&hasher.finalize())
}

/// Reads a fresh token from the kernel.
///
/// # Errors
/// Fails only if `/dev/urandom` cannot be read, which means `/dev` is not mounted — a
/// far larger problem than a missing token.
pub fn generate() -> std::io::Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    std::fs::File::open(RANDOM_SOURCE)?.read_exact(&mut bytes)?;
    Ok(encode(&bytes))
}

/// Compares a presented token against a stored fingerprint without leaking where they
/// differ.
///
/// The obvious `==` on strings returns as soon as two bytes differ, and the time it
/// takes is therefore a measurement of how much of the secret the caller already has.
/// Over a LAN that is a slow attack and a real one. This looks at every byte whatever
/// happens.
///
/// Lengths are compared first and short-circuit, which is safe: the length of a SHA-256
/// digest is not a secret.
#[must_use]
pub fn matches(presented: &str, stored_fingerprint: &str) -> bool {
    let candidate = fingerprint(presented);
    let expected = stored_fingerprint.trim();
    if candidate.len() != expected.len() {
        return false;
    }
    let differences = candidate
        .bytes()
        .zip(expected.bytes())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b));
    differences == 0
}

/// The state of the device's credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// A fingerprint is on file; the device has been claimed.
    Set(String),
    /// No fingerprint. The device is unclaimed and a token must be generated.
    Unset,
}

/// Reads the stored fingerprint.
///
/// A missing file means unclaimed, not broken: that is the state of every device on its
/// first boot. An unreadable or empty one is treated the same way, deliberately —
/// leaving the console permanently unauthenticatable because a file was truncated by a
/// power cut would be a worse failure than reclaiming the device from the console.
#[must_use]
pub fn read(path: &Path) -> Credential {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let line = contents.trim();
            if line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit()) {
                Credential::Set(line.to_ascii_lowercase())
            } else {
                Credential::Unset
            }
        }
        Err(_) => Credential::Unset,
    }
}

/// Writes a fingerprint, readable only by the user that owns it.
///
/// # Errors
/// Any failure to write or to set the mode. The mode is not decorative: `/var` holds
/// state that other things read, and a world-readable fingerprint would hand anyone
/// with a shell the ability to test guesses offline at whatever rate they like.
pub fn write(path: &Path, fingerprint: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{fingerprint}\n"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Extracts a token from an `Authorization: Bearer <token>` header value.
///
/// Returns `None` for any other scheme rather than trying to be helpful. A console that
/// accepted the token in a query string would put it in every proxy log and browser
/// history on the way.
#[must_use]
pub fn bearer(header_value: &str) -> Option<&str> {
    let rest = header_value.strip_prefix("Bearer ")?;
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token is 64 hex characters; this is a convenient one that is not secret.
    const SAMPLE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn the_fingerprint_is_sha256_of_the_token_text() {
        // Pinned against a value from a different implementation — `printf %s ... |
        // sha256sum` — rather than against this module's own output, which would agree
        // with itself however wrong it was.
        assert_eq!(
            fingerprint("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "the NIST test vector for SHA-256 of \"abc\""
        );
    }

    #[test]
    fn a_generated_token_has_the_entropy_the_adr_claims() {
        let token = generate().expect("/dev/urandom");
        assert_eq!(token.len(), TOKEN_BYTES * 2, "hex doubles the byte count");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn two_generated_tokens_differ() {
        // Catches the whole class of mistake where the "random" source is a constant,
        // a zeroed buffer, or a read that returned nothing and was not checked.
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a, b);
        assert_ne!(a, "0".repeat(TOKEN_BYTES * 2), "not a zeroed buffer");
    }

    #[test]
    fn the_right_token_matches_and_a_wrong_one_does_not() {
        let stored = fingerprint(SAMPLE);
        assert!(matches(SAMPLE, &stored));
        assert!(!matches("wrong", &stored));
    }

    #[test]
    fn a_token_differing_in_the_last_character_is_rejected() {
        // The case a comparison that stops early gets right and slowly.
        let stored = fingerprint(SAMPLE);
        let mut nearly = SAMPLE.to_owned();
        nearly.pop();
        nearly.push('0');
        assert_ne!(nearly, SAMPLE);
        assert!(!matches(&nearly, &stored));
    }

    #[test]
    fn the_stored_fingerprint_is_not_the_token() {
        // The property that makes a stolen copy of /var useless: what is on disk cannot
        // be presented to the console.
        let stored = fingerprint(SAMPLE);
        assert_ne!(stored, SAMPLE);
        assert!(
            !matches(&stored, &stored),
            "the digest is not a valid token"
        );
    }

    #[test]
    fn a_missing_file_means_unclaimed_rather_than_broken() {
        // Every device is in this state on its first boot.
        let missing = std::env::temp_dir().join("plexos-auth-not-here");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(read(&missing), Credential::Unset);
    }

    #[test]
    fn a_truncated_file_means_unclaimed_rather_than_locked_out() {
        // A power cut mid-write. Refusing to authenticate for ever because of half a
        // line would be a worse failure than letting the device be reclaimed from the
        // console it is attached to.
        let path = std::env::temp_dir().join("plexos-auth-truncated");
        std::fs::write(&path, "abc123\n").unwrap();
        assert_eq!(read(&path), Credential::Unset);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_written_fingerprint_reads_back_and_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join("plexos-auth-roundtrip");
        let _ = std::fs::remove_file(&path);
        let stored = fingerprint(SAMPLE);
        write(&path, &stored).unwrap();

        assert_eq!(read(&path), Credential::Set(stored.clone()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a readable fingerprint permits offline guessing"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_bearer_header_yields_the_token() {
        assert_eq!(bearer(&format!("Bearer {SAMPLE}")), Some(SAMPLE));
        assert_eq!(bearer("Bearer   "), None);
    }

    #[test]
    fn other_schemes_are_not_accepted() {
        // Basic in particular: accepting it would mean base64, a username nobody has,
        // and a second code path to get wrong.
        assert_eq!(bearer("Basic dXNlcjpwYXNz"), None);
        assert_eq!(bearer(SAMPLE), None);
        assert_eq!(bearer("bearer lowercase"), None);
    }
}
