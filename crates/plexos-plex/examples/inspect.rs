//! Runs the whole read-and-verify path over a real `.deb` and prints the result.
//!
//! The unit tests pin the parsers against captured fragments, which is necessary and
//! not sufficient: a fragment is still something this project chose. This runs the same
//! code over a whole artefact straight from Plex, so the answer can be compared against
//! `ar t` and `gpg --decrypt` rather than against our own fixtures.
//!
//! ```text
//! cargo run -p plexos-plex --example inspect -- package.deb [keyring.gpg]
//! ```
//!
//! Without a keyring it reads and reports, and says plainly that nothing was verified.
//! With one it performs the real check — signature, then every member against the
//! signed manifest — and exits non-zero if the package should not be installed.
//!
//! It never writes and never unpacks.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Reads one member's bytes out of the archive at the offset the directory reported.
fn member_bytes(file: &mut File, member: &plexos_plex::ar::Member) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(member.offset))?;
    let mut raw = vec![0_u8; usize::try_from(member.size).expect("member fits in memory")];
    file.read_exact(&mut raw)?;
    Ok(raw)
}

/// SHA1 of a member, via the same `sha1sum` the appliance carries.
///
/// Shelling out rather than adding a hash crate: the digest is Plex's choice, not ours,
/// and busybox already provides it in the image. Hashing 83 MB through a pipe costs
/// about as long as reading it.
fn sha1_of(path: &Path) -> std::io::Result<String> {
    let out = std::process::Command::new("sha1sum").arg(path).output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}

fn main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: inspect <package.deb> [keyring.gpg]");
        return Ok(ExitCode::from(64));
    };
    let keyring = args.next().map(PathBuf::from);

    let mut file = File::open(&path)?;
    let members = plexos_plex::ar::directory(&mut file)?;

    println!("{path}");
    for member in &members {
        println!(
            "  {:<16} {:>10} bytes at offset {}",
            member.name, member.size, member.offset
        );
    }

    let Some(signature) = members
        .iter()
        .find(|m| m.name == plexos_plex::SIGNATURE_MEMBER)
    else {
        println!("\nno {} member: unsigned", plexos_plex::SIGNATURE_MEMBER);
        return Ok(ExitCode::FAILURE);
    };

    let Some(keyring) = keyring else {
        println!("\nno keyring given, so nothing was verified.");
        println!("Pass one to check the signature: inspect {path} <keyring.gpg>");
        return Ok(ExitCode::SUCCESS);
    };

    // gpgv reads a file, so the signature member is written out first. A temporary
    // beside the package rather than in /tmp, which the appliance's root did not have
    // until recently and which is a lesson worth not relearning.
    let scratch = std::env::temp_dir().join("plexos-inspect-signature.asc");
    let raw = member_bytes(&mut file, signature)?;
    File::create(&scratch)?.write_all(&raw)?;

    let body = match plexos_plex::verify::clearsigned(&scratch, &keyring) {
        Ok(body) => body,
        Err(error) => {
            println!("\nSIGNATURE REJECTED\n  {error}");
            let _ = std::fs::remove_file(&scratch);
            return Ok(ExitCode::FAILURE);
        }
    };
    let _ = std::fs::remove_file(&scratch);

    let manifest = plexos_plex::manifest::parse(&body)?;
    println!("\nsignature verified against {}", keyring.display());
    println!("  signer: {}", manifest.signer);

    // Every member measured from the archive itself, then compared with the manifest.
    // This is the step that makes the signature mean something about these bytes.
    let mut measured = Vec::new();
    for member in &members {
        let extracted = std::env::temp_dir().join(format!("plexos-inspect-{}", member.name));
        File::create(&extracted)?.write_all(&member_bytes(&mut file, member)?)?;
        measured.push(plexos_plex::Measured {
            name: member.name.clone(),
            size: member.size,
            sha1: sha1_of(&extracted)?,
        });
        let _ = std::fs::remove_file(&extracted);
    }

    let problems = plexos_plex::agrees_with(&measured, &manifest);
    if problems.is_empty() {
        println!("\nall {} members match the signed manifest", members.len());
        Ok(ExitCode::SUCCESS)
    } else {
        println!("\nPACKAGE DOES NOT MATCH ITS SIGNATURE");
        for problem in &problems {
            println!("  {problem}");
        }
        Ok(ExitCode::FAILURE)
    }
}
