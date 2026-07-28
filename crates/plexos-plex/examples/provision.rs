//! Runs a whole provisioning cycle over a real package, into a directory you name.
//!
//! ```text
//! cargo run -p plexos-plex --example provision -- <package.deb> <keyring.gpg> <apps-dir>
//! ```
//!
//! Verifies, then unpacks, builds the erofs image, records its SHA256, publishes it and
//! moves `current`. The same code the appliance will run, pointed somewhere harmless so
//! the whole path can be exercised before it is trusted with `/var`.
//!
//! Refuses to run without a keyring. An unverified provisioning run is the one thing
//! this crate exists to prevent, and a convenience flag to skip it would eventually be
//! used.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use plexos_plex::{ar, build, execute, manifest, store, tools, verify};

fn member_bytes(file: &mut File, member: &ar::Member) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(member.offset))?;
    let mut raw = vec![0_u8; usize::try_from(member.size).expect("fits")];
    file.read_exact(&mut raw)?;
    Ok(raw)
}

fn sha1_of(tools: &tools::Tools, path: &Path) -> std::io::Result<String> {
    // sha1sum sits beside sha256sum; resolved the same way and for the same reason.
    let sha1 = tools
        .sha256sum
        .parent()
        .map_or_else(|| PathBuf::from("sha1sum"), |dir| dir.join("sha1sum"));
    let out = std::process::Command::new(sha1).arg(path).output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(package), Some(keyring), Some(apps)) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: provision <package.deb> <keyring.gpg> <apps-dir>");
        return Ok(ExitCode::from(64));
    };
    let package = PathBuf::from(package);
    let apps = PathBuf::from(apps);
    std::fs::create_dir_all(&apps)?;

    let tools = find_tools()?;
    println!(
        "tools: {}, {}, {}",
        tools.tar.display(),
        tools.mkfs_erofs.display(),
        tools.sha256sum.display()
    );

    let mut file = File::open(&package)?;
    let members = ar::directory(&mut file)?;

    // 1. Verify the signature.
    let signature = members
        .iter()
        .find(|m| m.name == plexos_plex::SIGNATURE_MEMBER)
        .ok_or("the package is unsigned")?;
    let scratch = apps.join(".signature");
    File::create(&scratch)?.write_all(&member_bytes(&mut file, signature)?)?;
    let body = verify::clearsigned(&scratch, Path::new(&keyring))?;
    std::fs::remove_file(&scratch)?;
    let signed = manifest::parse(&body)?;
    println!("signature: verified, signer {}", signed.signer);

    // 2. Tie it to these bytes.
    let mut measured = Vec::new();
    for member in &members {
        let extracted = apps.join(format!(".measure-{}", member.name));
        File::create(&extracted)?.write_all(&member_bytes(&mut file, member)?)?;
        measured.push(plexos_plex::Measured {
            name: member.name.clone(),
            size: member.size,
            sha1: sha1_of(&tools, &extracted)?,
        });
        std::fs::remove_file(&extracted)?;
    }
    let problems = plexos_plex::agrees_with(&measured, &signed);
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        return Ok(ExitCode::FAILURE);
    }
    println!("members:   all {} match the signed manifest", members.len());

    // 3. Only now: build and install.
    let control = members
        .iter()
        .find(|m| m.name == "control.tar.xz")
        .ok_or("no control member")?;
    let version = version_from_control(&tools, &mut file, control, &apps)?;
    println!("version:   {}", version.raw);

    let data = members
        .iter()
        .find(|m| m.name == "data.tar.xz")
        .ok_or("no data member")?;
    let layout = build::Layout { apps: apps.clone() };

    let listing: Vec<String> = std::fs::read_dir(&apps)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let current = std::fs::read_link(layout.current())
        .ok()
        .map(|t| t.to_string_lossy().into_owned());
    let existing = store::Store::from_listing(&listing, current.as_deref());
    let superseded = existing.superseded(&version);

    let steps = build::install_plan(&layout, &version, &package, data, &superseded);
    let started = std::time::Instant::now();
    execute::plan(&steps, &tools, &mut |line| println!("  {line}"))?;
    println!("installed in {:.1}s", started.elapsed().as_secs_f32());

    let image = layout.image(&version);
    println!(
        "image:     {} ({} bytes)",
        image.display(),
        image.metadata()?.len()
    );
    println!(
        "record:    {}",
        std::fs::read_to_string(layout.record(&version))?.trim()
    );
    println!(
        "current -> {}",
        std::fs::read_link(layout.current())?.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Resolves the tools, letting `PLEXOS_TOOLS_DIR` *extend* the search.
///
/// For this example alone. On a build host mkfs.erofs lives in Buildroot's output tree
/// rather than /usr/sbin, while tar and sha256sum are where they always are.
/// `Tools::find` stays strict for the daemon: an override there would eventually be
/// used to paper over an incomplete image rather than to test one.
fn find_tools() -> Result<tools::Tools, Box<dyn std::error::Error>> {
    let extra = std::env::var_os("PLEXOS_TOOLS_DIR").map(PathBuf::from);
    let one = |name: &str| -> Result<PathBuf, Box<dyn std::error::Error>> {
        if let Some(found) = tools::resolve(name, &|p: &Path| p.exists()) {
            return Ok(found);
        }
        if let Some(candidate) = extra.as_ref().map(|dir| dir.join(name))
            && candidate.exists()
        {
            return Ok(candidate);
        }
        Err(format!("{name} is not installed, and not in PLEXOS_TOOLS_DIR either").into())
    };
    Ok(tools::Tools {
        tar: one("tar")?,
        mkfs_erofs: one("mkfs.erofs")?,
        sha256sum: one("sha256sum")?,
        sha1sum: one("sha1sum")?,
        losetup: one("losetup")?,
    })
}

/// Reads the upstream version out of `control.tar.xz`.
fn version_from_control(
    tools: &tools::Tools,
    file: &mut File,
    control: &ar::Member,
    scratch_dir: &Path,
) -> Result<store::Version, Box<dyn std::error::Error>> {
    let raw = member_bytes(file, control)?;
    let path = scratch_dir.join(".control.tar.xz");
    File::create(&path)?.write_all(&raw)?;
    let out = std::process::Command::new(&tools.tar)
        .arg("-xJO")
        .arg("-f")
        .arg(&path)
        .arg("./control")
        .output()?;
    std::fs::remove_file(&path)?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("Version:"))
        .ok_or("control has no Version field")?;
    store::Version::parse(line.trim()).ok_or_else(|| "unparsable version".into())
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("provisioning failed: {error}");
            ExitCode::FAILURE
        }
    }
}
