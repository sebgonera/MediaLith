//! Reads a real `.deb` and prints what this crate makes of it.
//!
//! The unit tests pin the parsers against captured fragments, which is necessary and
//! not sufficient: a captured fragment is still a fragment this project chose. This
//! runs the same code over a whole artefact straight from Plex, so the answer can be
//! compared against `ar t` and `gpg --decrypt` rather than against our own fixtures.
//!
//! ```text
//! cargo run -p plexos-plex --example inspect -- plexmediaserver_x.y.z_amd64.deb
//! ```
//!
//! It reads and does not write, and it verifies nothing — signature checking is not
//! wired up yet. Do not mistake a clean run here for a trustworthy package.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: inspect <package.deb>");
        std::process::exit(64);
    };

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
        return Ok(());
    };

    // The clearsigned document, read straight out of the archive at the offset the
    // directory reported. If that offset is wrong this will not parse, which is the
    // point of doing it this way rather than trusting `ar x`.
    file.seek(SeekFrom::Start(signature.offset))?;
    let mut raw = vec![0_u8; usize::try_from(signature.size)?];
    file.read_exact(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);

    // Strip the clearsign armour. This is emphatically NOT verification: it is how the
    // parser gets something to read while gpgv is not yet wired in, and the printed
    // warning is there so no one reads this output as a trust decision.
    let mut body = String::new();
    for line in text
        .lines()
        .skip_while(|l| !l.starts_with("Version:"))
        .take_while(|l| !l.starts_with("-----BEGIN PGP SIGNATURE"))
    {
        body.push_str(line);
        body.push('\n');
    }

    let manifest = plexos_plex::manifest::parse(&body)?;
    println!("\nmanifest (UNVERIFIED — signature not checked)");
    println!("  signer: {}", manifest.signer);
    println!("  role:   {}", manifest.role);
    for entry in &manifest.entries {
        println!("  {:<16} {:>10} {}", entry.name, entry.size, entry.sha1);
    }

    Ok(())
}
