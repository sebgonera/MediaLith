//! The `ar` archive a Debian package is, read without unpacking it.
//!
//! A `.deb` is a plain `ar` archive of three or four members, in order:
//! `debian-binary`, `control.tar.<c>`, `data.tar.<c>`, and — for Plex, and for anything
//! else signed with `debsigs` — a signature member. Reading the directory means walking
//! fixed-size headers and skipping payloads, so the 83 MB of `data.tar.xz` is never
//! held in memory and never touched before its signature has been checked.
//!
//! That ordering is the point. Everything here runs *before* verification, so it must
//! not be able to do anything with the contents beyond locating them.

use std::io::{self, Read, Seek, SeekFrom};

/// The magic an `ar` archive starts with.
const MAGIC: &[u8; 8] = b"!<arch>\n";

/// Length of one member header.
const HEADER_LEN: usize = 60;

/// Offset of the size field within a header, and its width.
const SIZE_AT: usize = 48;
const SIZE_WIDTH: usize = 10;

/// Where the member name starts, and how wide it is.
const NAME_WIDTH: usize = 16;

/// The largest archive directory this will walk before giving up.
///
/// A `.deb` has three or four members. Anything claiming hundreds is not a package
/// this should be reading, and the bound means a corrupt header cannot spin.
const MAX_MEMBERS: usize = 64;

/// One member of the archive, located but not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Member name with `ar`'s trailing padding and slash removed, e.g. `data.tar.xz`.
    pub name: String,
    /// Payload length in bytes.
    pub size: u64,
    /// Byte offset of the payload from the start of the archive.
    pub offset: u64,
}

/// What can go wrong reading the directory.
#[derive(Debug)]
pub enum Error {
    /// The file does not begin with `!<arch>\n`.
    NotAnArchive,
    /// A header was truncated, or a field was not the expected shape.
    MalformedHeader {
        /// Which member, counting from zero.
        index: usize,
        /// What was wrong with it.
        detail: String,
    },
    /// More members than [`MAX_MEMBERS`].
    TooManyMembers,
    /// The underlying file could not be read.
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnArchive => write!(
                f,
                "not a Debian package: the file does not start with an ar archive header. \
                 A downloaded HTML error page is the usual cause — check the first bytes \
                 with `head -c 8`."
            ),
            Self::MalformedHeader { index, detail } => write!(
                f,
                "member {index} of the archive is malformed: {detail}. The download is \
                 damaged; fetch it again."
            ),
            Self::TooManyMembers => write!(
                f,
                "the archive claims more than {MAX_MEMBERS} members, which no Debian \
                 package has. Treating it as corrupt rather than reading further."
            ),
            Self::Io(error) => write!(f, "could not read the archive: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads the member directory, leaving the payloads where they are.
///
/// # Errors
/// See [`Error`]. Anything unreadable is reported rather than skipped: a `.deb` with a
/// member this cannot locate is one whose signature cannot be checked against every
/// member, and a partial check is worse than none.
pub fn directory(source: &mut (impl Read + Seek)) -> Result<Vec<Member>, Error> {
    let mut magic = [0_u8; MAGIC.len()];
    source.read_exact(&mut magic).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            Error::NotAnArchive
        } else {
            Error::Io(error)
        }
    })?;
    if &magic != MAGIC {
        return Err(Error::NotAnArchive);
    }

    let mut members = Vec::new();
    let mut offset = MAGIC.len() as u64;

    loop {
        // Filled by hand rather than with read_exact, which cannot tell the two ends
        // apart: it reports UnexpectedEof both for a clean finish after the last
        // payload and for a header cut in half by a truncated download. Treating the
        // second as the first returns a short directory from a damaged file, and the
        // signature then gets checked against the members that happened to survive.
        let mut header = [0_u8; HEADER_LEN];
        let mut filled = 0;
        while filled < HEADER_LEN {
            match source.read(&mut header[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            break;
        }
        if filled < HEADER_LEN {
            return Err(Error::MalformedHeader {
                index: members.len(),
                detail: format!(
                    "the archive ends {filled} bytes into a {HEADER_LEN}-byte header, so \
                     it is truncated"
                ),
            });
        }
        offset += HEADER_LEN as u64;

        if members.len() >= MAX_MEMBERS {
            return Err(Error::TooManyMembers);
        }

        let index = members.len();
        let name = header[..NAME_WIDTH]
            .iter()
            .map(|b| char::from(*b))
            .collect::<String>();
        // ar pads names with spaces, and GNU ar terminates them with a slash.
        let name = name.trim_end().trim_end_matches('/').to_owned();
        if name.is_empty() {
            return Err(Error::MalformedHeader {
                index,
                detail: "the member has no name".to_owned(),
            });
        }

        let size_field = std::str::from_utf8(&header[SIZE_AT..SIZE_AT + SIZE_WIDTH])
            .map_err(|_| Error::MalformedHeader {
                index,
                detail: format!("the size field of {name} is not text"),
            })?
            .trim();
        let size: u64 = size_field.parse().map_err(|_| Error::MalformedHeader {
            index,
            detail: format!("the size field of {name} reads {size_field:?}, which is not a number"),
        })?;

        members.push(Member { name, size, offset });

        // Payloads are padded to an even boundary, and the padding byte is not counted
        // in the size. Skipping it is what keeps the next header aligned; forgetting it
        // reads a header one byte late and every field is then garbage.
        let padded = size + (size % 2);
        offset += padded;
        source.seek(SeekFrom::Start(offset))?;
    }

    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Builds one ar member header plus payload, the way `ar` writes it.
    fn member(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("{name:<16}").as_bytes());
        out.extend_from_slice(b"1758208670  0     0     100644  ");
        out.extend_from_slice(format!("{:<10}", payload.len()).as_bytes());
        out.extend_from_slice(b"`\n");
        assert_eq!(out.len(), HEADER_LEN, "header must be exactly 60 bytes");
        out.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            out.push(b'\n');
        }
        out
    }

    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        for (name, payload) in members {
            out.extend_from_slice(&member(name, payload));
        }
        out
    }

    #[test]
    fn a_real_deb_header_is_read_as_plex_writes_it() {
        // The first 132 bytes of plexmediaserver_1.42.2.10156_amd64.deb, captured on
        // 2026-07-27. Pinned against the actual artefact rather than against this
        // module's own writer, because a test that only round-trips `member()` above
        // would pass just as well if both sides agreed on the wrong layout.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"!<arch>\n");
        raw.extend_from_slice(b"debian-binary   1758208670  0     0     100644  4         `\n");
        raw.extend_from_slice(b"2.0\n");
        raw.extend_from_slice(b"control.tar.xz  1758208670  0     0     100644  74256     `\n");

        let found = directory(&mut Cursor::new(raw)).unwrap();
        assert_eq!(found[0].name, "debian-binary");
        assert_eq!(found[0].size, 4);
        assert_eq!(
            found[0].offset, 68,
            "8 bytes of magic, then a 60-byte header"
        );
        assert_eq!(found[1].name, "control.tar.xz");
        assert_eq!(found[1].size, 74256);
    }

    #[test]
    fn the_four_members_of_a_signed_package_are_all_found() {
        let raw = archive(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar.xz", b"control"),
            ("data.tar.xz", b"data"),
            ("_gpgplex", b"signature"),
        ]);
        let names: Vec<String> = directory(&mut Cursor::new(raw))
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(
            names,
            ["debian-binary", "control.tar.xz", "data.tar.xz", "_gpgplex"]
        );
    }

    #[test]
    fn an_odd_length_payload_does_not_shift_the_next_header() {
        // ar pads to an even boundary without counting the pad in the size. Reading the
        // next header one byte early turns every field after it into nonsense, and the
        // symptom is a member with an unparsable size rather than anything naming
        // alignment.
        let raw = archive(&[("debian-binary", b"2.0\n"), ("odd.tar.xz", b"xyz")]);
        let found = directory(&mut Cursor::new(raw.clone())).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].size, 3);

        // And the offset it reports really is where the payload sits.
        let start = usize::try_from(found[1].offset).unwrap();
        assert_eq!(&raw[start..start + 3], b"xyz");
    }

    #[test]
    fn an_html_error_page_is_named_as_such_rather_than_parsed() {
        // The realistic failure: a proxy or a captive portal answers the download with
        // a page. "not a Debian package" plus where to look beats a parse error about
        // byte 48.
        let error = directory(&mut Cursor::new(b"<!doctype html>\n<html>".to_vec())).unwrap_err();
        assert!(matches!(error, Error::NotAnArchive));
        assert!(
            error.to_string().contains("head -c 8"),
            "names a remedy: {error}"
        );
    }

    #[test]
    fn a_truncated_download_is_an_error_and_not_a_short_directory() {
        // Half a header. Reporting two members and moving on would mean verifying a
        // signature against a member list that is missing the payload.
        let mut raw = archive(&[("debian-binary", b"2.0\n")]);
        raw.extend_from_slice(b"data.tar.xz     1758");
        let error = directory(&mut Cursor::new(raw)).unwrap_err();
        assert!(
            matches!(error, Error::MalformedHeader { .. }),
            "a half-read header is truncation, not the end of the archive: {error:?}"
        );
        assert!(error.to_string().contains("truncated"), "{error}");
    }

    #[test]
    fn a_nonsense_size_field_names_the_member_it_came_from() {
        let mut raw = MAGIC.to_vec();
        let mut header = member("data.tar.xz", b"");
        header[SIZE_AT..SIZE_AT + 5].copy_from_slice(b"////_");
        raw.extend_from_slice(&header);
        let error = directory(&mut Cursor::new(raw)).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("data.tar.xz"), "{message}");
    }
}
