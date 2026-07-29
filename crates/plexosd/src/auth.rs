//! The device token, and what it does and does not protect (ADR-0013).
//!
//! One appliance, one administrator, one secret. There is no user database, no password
//! and no default credential: the token is 80 bits from `/dev/urandom`, generated on the
//! machine at first start and shown on the console attached to it. Whoever can read that
//! screen may claim the device, which is the same trust model as a router's reset button
//! and is stated so that it is a decision rather than an oversight.
//!
//! # Why a token and not a password
//!
//! A key-derivation function exists to make guessing a low-entropy secret expensive.
//! Against 80 bits of entropy there is nothing to guess — a million attempts a second,
//! which is orders of magnitude beyond what this server could be made to answer, would
//! take longer than the age of the universe — so a single SHA-256 is sufficient and no
//! KDF, and therefore no hand-written cryptography, is needed. What hashing does buy is
//! that a copy of `/var`, from a backup or a pulled disk, yields nothing that can be
//! presented to the console.
//!
//! # Why it is short, and shaped the way it is
//!
//! The token is read off a 2160x1440 laptop panel in a console font and typed into a
//! browser on another machine. That is its whole life, and it is what the format is for:
//! sixteen characters in four groups, from an alphabet with no `I`, `L`, `O` or `U`, so
//! there is no pair to confuse and nothing to spell. [`normalise`] accepts it in any
//! case, with or without the dashes, and folds `O` onto `0` and `I`/`L` onto `1` — so a
//! reader who saw the wrong one of a pair is not punished for it.
//!
//! It began as 64 hex characters. Entropy nobody can transcribe is not security; it is
//! an obstacle to the only person entitled to get past it.
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

/// Where the device's credential lives.
///
/// Under the state root, so it survives an OS update and a rollback -- claiming a
/// device again after every update would be absurd, and ADR-0009 permits an addition
/// like this because a release that has never heard of the file simply ignores it.
pub const CREDENTIAL_FILE: &str = "/var/lib/plexos/device-token";

/// Bytes of entropy in a token.
///
/// 10, which is 80 bits. That is not a compromise between security and convenience: at
/// a million guesses a second — far beyond what this hand-written HTTP server could be
/// made to answer — exhausting 2^80 takes longer than the age of the universe. Guessing
/// is still not a threat model, so the absence of a lockout still needs no further
/// defence.
///
/// It was 32 bytes, and 64 hex characters turned out to be the wrong shape for the one
/// place this is read: a 2160x1440 laptop panel with a console font, transcribed by
/// hand into a browser on another machine. Entropy nobody can type is not security, it
/// is an obstacle to the person who owns the device.
pub const TOKEN_BYTES: usize = 10;

/// Characters in a token, which is [`TOKEN_BYTES`] at five bits each.
pub const TOKEN_CHARS: usize = TOKEN_BYTES * 8 / 5;

/// Characters per group when a token is shown to a person.
const GROUP: usize = 4;

/// Crockford's base32 alphabet.
///
/// Not RFC 4648's. The difference is the point: `I`, `L`, `O` and `U` are absent, so
/// there is no pair a person can confuse when reading one off a screen — no `O` to
/// mistake for `0`, no `l` for `1` — and no combination of the remainder spells anything
/// unfortunate. Upper case because a console font makes it the more legible of the two,
/// and [`normalise`] means it does not matter what is typed.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Where the kernel's randomness comes from.
///
/// `/dev/urandom` rather than `/dev/random`: on Linux they draw from the same pool once
/// it is initialised, and the appliance generates this after the health gate has run —
/// seconds into userspace, long past initialisation. `/dev/random` would add the
/// possibility of blocking forever on a machine with no entropy sources for nothing.
const RANDOM_SOURCE: &str = "/dev/urandom";

/// Bytes as lower-case hex.
///
/// This is the *stored* form — the fingerprint — and not what anybody types. It stays
/// hex because a SHA-256 digest is read by machines only.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// A token as the administrator sees it: [`ALPHABET`], upper case, no separators.
///
/// The canonical form. [`grouped`] adds dashes for display and [`normalise`] takes any
/// of it back to this.
#[must_use]
pub fn encode_token(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(TOKEN_CHARS);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;

    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((buffer >> bits) & 0x1f) as usize;
            out.push(ALPHABET[index] as char);
        }
    }
    // Only reachable if TOKEN_BYTES stops being a multiple of five bits. Kept so that
    // changing it produces a shorter token rather than a silently truncated one.
    if bits > 0 {
        let index = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}

/// The token as it is printed: groups of [`GROUP`] separated by dashes.
///
/// Grouping is not decoration. Sixteen unbroken characters are read by counting; four
/// groups of four are read by shape, and the reader can keep their place after looking
/// away at a keyboard.
#[must_use]
pub fn grouped(token: &str) -> String {
    token
        .chars()
        .collect::<Vec<_>>()
        .chunks(GROUP)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Takes anything a person might type back to the canonical form.
///
/// Upper-cases, drops separators and whitespace, and folds the characters the alphabet
/// deliberately excludes onto the ones they are mistaken for: `O` is a zero, `I` and `L`
/// are ones. Anything else is dropped, which makes a token containing a genuinely wrong
/// character fail to match rather than match something else.
///
/// This is why the token can be typed in any case, with or without the dashes, and why a
/// reader who saw `O` where a `0` was printed is not punished for it.
#[must_use]
pub fn normalise(input: &str) -> String {
    input
        .chars()
        .filter_map(|c| match c.to_ascii_uppercase() {
            'O' => Some('0'),
            'I' | 'L' => Some('1'),
            upper if ALPHABET.contains(&(upper as u8)) => Some(upper),
            _ => None,
        })
        .collect()
}

/// The stored form of a token.
///
/// The token is normalised first, so that what is stored does not depend on how the
/// administrator happened to type it. Both sides of [`matches`] go through here, which
/// is what keeps that true rather than merely intended.
#[must_use]
pub fn fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalise(token).as_bytes());
    hex(&hasher.finalize())
}

/// Reads a fresh token from the kernel.
///
/// # Errors
/// Fails only if `/dev/urandom` cannot be read, which means `/dev` is not mounted — a
/// far larger problem than a missing token.
pub fn generate() -> std::io::Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    std::fs::File::open(RANDOM_SOURCE)?.read_exact(&mut bytes)?;
    Ok(encode_token(&bytes))
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

/// The credential the running server is checking against.
///
/// Held here rather than captured by [`crate::http::serve`] so that rotation takes effect
/// at once. The distinction that matters is *who* may change it: the original reasoning
/// was that re-reading [`CREDENTIAL_FILE`] on every request would let a file replaced
/// out-of-band become the credential without a restart, which is a way past the console
/// rather than through it. That still holds — nothing here re-reads the file. What
/// changed is that the console may now deliberately swap the value, which is the whole
/// point of being able to rotate: a token rotated because it leaked has to stop working
/// now, not at the next reboot.
static CURRENT: std::sync::RwLock<Option<Credential>> = std::sync::RwLock::new(None);

/// Installs the credential the server will check against.
pub fn install(credential: Credential) {
    *CURRENT
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(credential);
}

/// The credential in force.
///
/// [`Credential::Unset`] until [`install`] has run, which is the safe direction: an
/// unclaimed device refuses every mutating route rather than allowing them.
#[must_use]
pub fn current() -> Credential {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or(Credential::Unset)
}

/// Issues a new token, stores its fingerprint, and puts it in force immediately.
///
/// Returns the token itself, which is the only time it exists in a readable form — the
/// file holds a fingerprint, and there is deliberately no way to ask for it again.
///
/// # Errors
/// If randomness cannot be read or the credential file cannot be written. The credential
/// in force is left alone in that case, so a failed rotation does not lock anybody out.
pub fn rotate(path: &Path) -> std::io::Result<String> {
    let token = generate()?;
    let print = fingerprint(&token);

    // Written before it is installed. A machine that loses power between the two comes
    // back checking the new token, which the person who asked for it has; the other order
    // would come back checking a token nobody was ever shown.
    write(path, &print)?;
    install(Credential::Set(print));

    Ok(token)
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

    #[test]
    fn a_rotated_token_is_in_force_immediately_and_the_old_one_is_not() {
        // The whole reason rotation exists. A token replaced because it leaked and then
        // honoured until the next reboot has not been revoked, it has been scheduled for
        // revocation -- and what it guards is a root shell.
        let path = std::env::temp_dir().join("plexos-auth-rotate");
        let _ = std::fs::remove_file(&path);

        let old = "AAAA-BBBB-CCCC-DDDD";
        install(Credential::Set(fingerprint(old)));

        let new = rotate(&path).expect("issues a token");
        assert_ne!(
            normalise(&new),
            normalise(old),
            "a fresh token, not the old one"
        );

        let Credential::Set(in_force) = current() else {
            panic!("rotation must leave a credential in force");
        };

        assert!(matches(&new, &in_force), "the token just issued must work");
        assert!(
            !matches(old, &in_force),
            "and the one it replaced must not -- otherwise nothing was revoked"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_token_is_readable_once_and_the_file_holds_only_its_fingerprint() {
        // There is deliberately no route that says it again. If the browser that asked
        // loses the reply, the remedy is another rotation, not a lookup.
        let path = std::env::temp_dir().join("plexos-auth-once");
        let _ = std::fs::remove_file(&path);

        let token = rotate(&path).expect("issues a token");
        let stored = std::fs::read_to_string(&path).expect("the file exists");

        assert!(
            !stored.contains(&normalise(&token)),
            "the token itself must never reach the disk: {stored:?}"
        );
        assert!(stored.trim().len() >= 32, "a fingerprint, not a token");

        let _ = std::fs::remove_file(&path);
    }

    /// A token shaped like a real one. Not secret: it is in a public repository.
    const SAMPLE: &str = "4K7QM2XR9T8BHVWP";

    #[test]
    fn the_fingerprint_is_sha256_of_the_normalised_token() {
        // Pinned against a value from a different implementation — `printf %s ABC |
        // sha256sum` — rather than against this module's own output, which would agree
        // with itself however wrong it was.
        //
        // ABC and not abc: the token is normalised before it is hashed, so that what is
        // stored does not depend on how the administrator happened to type it. This test
        // pins both facts at once, and it caught the change when normalisation arrived.
        assert_eq!(
            fingerprint("abc"),
            "b5d4045c3f466fa91fe2cc6abe79232a1a57cdf104f7a26e716e0a1e2789df78",
            "SHA-256 of \"ABC\", computed by sha256sum"
        );
        assert_eq!(
            fingerprint(SAMPLE),
            "fe8da1592814bca5332729e156f4ba5e3d5031e8260d2fd7a5f2a037214c5495",
            "and of the sample token, likewise"
        );
    }

    #[test]
    fn a_token_is_accepted_however_it_was_typed() {
        // The whole point of the shape. Somebody reading four groups off a laptop panel
        // types them with dashes or without, in whatever case their keyboard was in.
        let stored = fingerprint(SAMPLE);
        for typed in [
            "4K7QM2XR9T8BHVWP",
            "4K7Q-M2XR-9T8B-HVWP",
            "4k7q-m2xr-9t8b-hvwp",
            "4K7Q M2XR 9T8B HVWP",
            "  4K7Q-M2XR-9T8B-HVWP  ",
        ] {
            assert!(matches(typed, &stored), "{typed:?} must be accepted");
        }
    }

    #[test]
    fn the_letters_the_alphabet_excludes_are_folded_onto_the_digits() {
        // A reader who saw O where a 0 was printed, or l where a 1 was, has made the
        // mistake the alphabet exists to prevent. Rejecting them would blame the reader
        // for an ambiguity the format claims not to have.
        assert_eq!(normalise("O0Il1"), "00111");
        assert_eq!(normalise("4K7Q-M2XR"), "4K7QM2XR");
    }

    #[test]
    fn the_alphabet_contains_nothing_a_reader_can_confuse() {
        // The property, asserted rather than assumed: the four characters Crockford
        // leaves out are the four that cause this.
        for excluded in [b'I', b'L', b'O', b'U'] {
            assert!(
                !ALPHABET.contains(&excluded),
                "{} is in the alphabet and should not be",
                excluded as char
            );
        }
        assert_eq!(ALPHABET.len(), 32, "five bits per character");
        let mut seen = ALPHABET.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 32, "and every character distinct");
    }

    #[test]
    fn a_token_is_shown_in_groups_and_accepted_without_them() {
        // Sixteen unbroken characters are read by counting; four groups of four are read
        // by shape, and the reader can keep their place after looking at a keyboard.
        assert_eq!(grouped(SAMPLE), "4K7Q-M2XR-9T8B-HVWP");
        assert_eq!(normalise(&grouped(SAMPLE)), SAMPLE);
    }

    #[test]
    fn the_encoding_agrees_with_crockfords_own_example() {
        // Pinned outside this module: Crockford's base32 maps five zero bytes to eight
        // zeros, and the alphabet's order is what makes 0x00..0x09 the digits.
        assert_eq!(encode_token(&[0; 10]), "0".repeat(16));
        assert_eq!(encode_token(&[0xff; 10]), "Z".repeat(16));
        // 0b00001_00010_00011_00100_00101_00110_00111_01000 -> "123456789" prefix rules
        assert_eq!(encode_token(&[0x00, 0x44, 0x32, 0x14, 0xc7]).len(), 8);
    }

    #[test]
    fn a_generated_token_has_the_entropy_the_adr_claims() {
        let token = generate().expect("/dev/urandom");
        assert_eq!(token.len(), TOKEN_CHARS, "five bits per character");
        assert_eq!(
            token.len(),
            16,
            "and sixteen is what a person is asked to type"
        );
        assert!(
            token.bytes().all(|b| ALPHABET.contains(&b)),
            "every character must come from the unambiguous alphabet: {token}"
        );
    }

    #[test]
    fn two_generated_tokens_differ() {
        // Catches the whole class of mistake where the "random" source is a constant,
        // a zeroed buffer, or a read that returned nothing and was not checked.
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a, b);
        assert_ne!(a, "0".repeat(TOKEN_CHARS), "not a zeroed buffer");
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
        nearly.push('2');
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
