//! The `debsigs` manifest Plex signs, and what it does and does not prove.
//!
//! Plex's packages carry a fourth `ar` member named `_gpgplex`. It is a clearsigned
//! document — not a detached signature over the payload — listing every other member
//! with its MD5, SHA1 and size:
//!
//! ```text
//! Version: 4
//! Signer: Plex Inc.
//! Date: Thu Sep 18 15:18:34 2025
//! Role: plex
//! Files:
//!     3cf9...3e5e 7959...84ad 4 debian-binary
//!     bfe6...e430 40a0...1571 74256 control.tar.xz
//!     36cb...b000 8b2f...dce3 83047296 data.tar.xz
//! ```
//!
//! (indented with a tab in the real document, shown with spaces here)
//!
//! # What the signature covers
//!
//! The signature covers **this text**, and nothing else. It says Plex Inc. asserted
//! these hashes. Tying it to the package on disk is a separate step: every member must
//! be present, listed, and hash to what the manifest claims. A verified signature over
//! a manifest that was never compared against the payload proves only that Plex once
//! signed something.
//!
//! # The digests are weak, and that is not fixable here
//!
//! MD5 and SHA1, with the clearsign digest itself SHA1. Both are broken for collision
//! resistance, and this crate cannot improve on what Plex publishes. What it can do is
//! record a SHA256 of the artefact it actually installed, which is what ADR-0010's
//! third point asks for and what makes a device's state auditable after the fact.
//! Origin comes from the signature; integrity over time comes from our own hash.

/// One line of the `Files:` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// MD5 of the member, as lower-case hex.
    pub md5: String,
    /// SHA1 of the member, as lower-case hex.
    pub sha1: String,
    /// Size in bytes.
    pub size: u64,
    /// Member name, matching the `ar` directory.
    pub name: String,
}

/// A parsed manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Who the document says signed it. Informational: the signature, not this field,
    /// is what establishes the signer.
    pub signer: String,
    /// The `Role` field, `plex` on every package seen.
    pub role: String,
    /// One entry per member.
    pub entries: Vec<Entry>,
}

/// Why a manifest could not be understood.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// No `Files:` block.
    NoFileList,
    /// A line inside `Files:` was not four whitespace-separated fields.
    MalformedEntry(String),
    /// A size was not a number.
    MalformedSize(String),
    /// A hash was not hex of the expected length.
    MalformedHash(String),
    /// The `Files:` block was empty.
    NoEntries,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFileList => write!(
                f,
                "the signed manifest has no `Files:` block, so it lists nothing to check \
                 the package against. Plex's packaging has changed shape; this is a \
                 release-blocking upstream change (ADR-0010), not a damaged download."
            ),
            Self::MalformedEntry(line) => write!(
                f,
                "a manifest entry is not `<md5> <sha1> <size> <name>`: {line:?}"
            ),
            Self::MalformedSize(field) => {
                write!(f, "a manifest entry has a size of {field:?}")
            }
            Self::MalformedHash(field) => write!(
                f,
                "a manifest entry has {field:?} where a hex digest belongs"
            ),
            Self::NoEntries => write!(
                f,
                "the manifest's `Files:` block is empty, so verifying the package \
                 against it would check nothing and pass"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Is this lower-case hex of exactly `width` characters?
fn is_hex(field: &str, width: usize) -> bool {
    field.len() == width && field.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parses the *body* of the manifest: the text a signature check has already covered.
///
/// Takes the verified plaintext rather than the clearsigned document on purpose. Making
/// the caller produce the verified text first means this cannot be handed an unverified
/// document by accident — there is no code path here that reads past a signature.
///
/// # Errors
/// See [`Error`]. Every failure means the package cannot be checked, so none of them is
/// recoverable by carrying on with fewer entries.
pub fn parse(body: &str) -> Result<Manifest, Error> {
    let mut signer = String::new();
    let mut role = String::new();
    let mut entries = Vec::new();
    let mut in_files = false;

    for line in body.lines() {
        if !in_files {
            if let Some(value) = line.strip_prefix("Signer:") {
                value.trim().clone_into(&mut signer);
            } else if let Some(value) = line.strip_prefix("Role:") {
                value.trim().clone_into(&mut role);
            } else if line.trim_end() == "Files:" {
                in_files = true;
            }
            continue;
        }

        // The block runs to the end of the document or to the first line that is not
        // indented. Blank lines end it too: a clearsigned body ends with one.
        if line.trim().is_empty() {
            break;
        }
        if !line.starts_with([' ', '\t']) {
            break;
        }

        let fields: Vec<&str> = line.split_whitespace().collect();
        let [md5, sha1, size, name] = fields.as_slice() else {
            return Err(Error::MalformedEntry(line.trim().to_owned()));
        };
        if !is_hex(md5, 32) {
            return Err(Error::MalformedHash((*md5).to_owned()));
        }
        if !is_hex(sha1, 40) {
            return Err(Error::MalformedHash((*sha1).to_owned()));
        }
        let size: u64 = size
            .parse()
            .map_err(|_| Error::MalformedSize((*size).to_owned()))?;

        entries.push(Entry {
            md5: (*md5).to_owned(),
            sha1: (*sha1).to_owned(),
            size,
            name: (*name).to_owned(),
        });
    }

    if !in_files {
        return Err(Error::NoFileList);
    }
    if entries.is_empty() {
        return Err(Error::NoEntries);
    }

    Ok(Manifest {
        signer,
        role,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verified body of `_gpgplex` from
    /// `plexmediaserver_1.42.2.10156-f737b826c_amd64.deb`, captured on 2026-07-27.
    ///
    /// A real document rather than one this module's tests invented: the shape of
    /// Plex's manifest is not ours to decide, and a fixture written from the parser's
    /// own assumptions would agree with them however wrong they were.
    const REAL: &str = "Version: 4\n\
        Signer: Plex Inc.\n\
        Date: Thu Sep 18 15:18:34 2025\n\
        Role: plex\n\
        Files: \n\
        \t3cf918272ffa5de195752d73f3da3e5e 7959c969e092f2a5a8604e2287807ac5b1b384ad 4 debian-binary\n\
        \tbfe69e3256945d8d1cb39522b943e430 40a00c89e9f1ac27f116379efbaa5b67ddeb1571 74256 control.tar.xz\n\
        \t36cbc776f8dc78c76146a8b3d54fb000 8b2fc4685435bd956d6e9b507d0e3bfbe503dce3 83047296 data.tar.xz\n";

    #[test]
    fn plexs_real_manifest_parses_to_its_three_members() {
        let manifest = parse(REAL).unwrap();
        assert_eq!(manifest.signer, "Plex Inc.");
        assert_eq!(manifest.role, "plex");
        assert_eq!(manifest.entries.len(), 3);

        let data = &manifest.entries[2];
        assert_eq!(data.name, "data.tar.xz");
        assert_eq!(data.size, 83_047_296);
        assert_eq!(data.sha1, "8b2fc4685435bd956d6e9b507d0e3bfbe503dce3");
    }

    #[test]
    fn the_files_header_is_matched_despite_its_trailing_space() {
        // Plex writes `Files: ` with a trailing space. Matching the line exactly is the
        // obvious way to find the block and it silently finds nothing here, leaving a
        // manifest that parses to zero entries and a check that passes vacuously.
        assert!(REAL.contains("Files: \n"), "the fixture keeps the space");
        assert_eq!(parse(REAL).unwrap().entries.len(), 3);
    }

    #[test]
    fn an_empty_file_list_is_refused_rather_than_accepted_as_nothing_to_check() {
        // The dangerous shape: a well-formed, correctly signed manifest listing no
        // files. Every member then matches every claim, because there are none.
        let error = parse("Signer: Plex Inc.\nFiles:\n").unwrap_err();
        assert_eq!(error, Error::NoEntries);
        assert!(
            error.to_string().contains("pass"),
            "says why it matters: {error}"
        );
    }

    #[test]
    fn a_manifest_with_no_file_list_at_all_is_refused() {
        let error = parse("Version: 4\nSigner: Plex Inc.\n").unwrap_err();
        assert_eq!(error, Error::NoFileList);
    }

    #[test]
    fn a_truncated_hash_is_refused_rather_than_compared_as_a_prefix() {
        // A short digest that happened to be a prefix of the real one would match
        // nothing, but the failure would look like a corrupt package rather than a
        // malformed manifest. Length is checked so the message stays accurate.
        let bad = "Files:\n\t3cf9 7959c969e092f2a5a8604e2287807ac5b1b384ad 4 debian-binary\n";
        assert!(matches!(parse(bad), Err(Error::MalformedHash(_))));
    }

    #[test]
    fn a_name_containing_spaces_is_refused_rather_than_silently_truncated() {
        // Four fields exactly. A fifth means the name has a space in it, and taking
        // only the fourth would verify a member with a different name from the one the
        // manifest meant.
        let bad = "Files:\n\t3cf918272ffa5de195752d73f3da3e5e \
                   7959c969e092f2a5a8604e2287807ac5b1b384ad 4 data file.tar\n";
        assert!(matches!(parse(bad), Err(Error::MalformedEntry(_))));
    }

    #[test]
    fn the_block_ends_at_the_first_unindented_line() {
        let trailing = format!("{REAL}Signature-Trailer: ignored\n");
        assert_eq!(parse(&trailing).unwrap().entries.len(), 3);
    }
}
