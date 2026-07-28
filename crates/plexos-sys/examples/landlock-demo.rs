//! Applies a Landlock policy to this process and then tries to break out of it.
//!
//! ```text
//! cargo run -p plexos-sys --example landlock-demo
//! ```
//!
//! The unit tests pin constants and struct layouts, which is necessary and proves
//! nothing about whether the kernel actually denies anything. This confines itself to
//! one directory and then reads a file inside it and a file outside it, reporting both.
//! Exits non-zero if either answer is wrong.
//!
//! It cannot be a `#[test]`: `landlock_restrict_self` is irreversible and inherited, so
//! a test that applied it would confine the whole test binary and every test after it.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use plexos_sys::landlock::{Ruleset, access};

fn main() -> ExitCode {
    let abi = match plexos_sys::landlock::abi_version() {
        Ok(abi) => abi,
        Err(error) => {
            eprintln!("Landlock unavailable: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("Landlock ABI {abi}");

    let inside = std::env::temp_dir().join("plexos-landlock-demo");
    let _ = std::fs::remove_dir_all(&inside);
    if let Err(error) = std::fs::create_dir_all(&inside) {
        eprintln!("could not prepare {}: {error}", inside.display());
        return ExitCode::FAILURE;
    }
    let permitted = inside.join("permitted.txt");
    if let Err(error) =
        std::fs::File::create(&permitted).and_then(|mut f| f.write_all(b"reachable\n"))
    {
        eprintln!("could not write {}: {error}", permitted.display());
        return ExitCode::FAILURE;
    }

    // A file outside the sandbox that certainly exists and is certainly readable now.
    let forbidden = Path::new("/etc/hostname");
    let readable_before = std::fs::read(forbidden).is_ok();
    println!(
        "before: {} readable = {readable_before}",
        forbidden.display()
    );
    if !readable_before {
        eprintln!(
            "this demo needs {} to be readable to start with",
            forbidden.display()
        );
        return ExitCode::FAILURE;
    }

    let mut ruleset = match Ruleset::new(access::ALL) {
        Ok(ruleset) => ruleset,
        Err(error) => {
            eprintln!("could not build a ruleset: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = ruleset.allow(&inside, access::READ_WRITE) {
        eprintln!("could not grant access to {}: {error}", inside.display());
        return ExitCode::FAILURE;
    }
    if let Err(error) = ruleset.enforce() {
        eprintln!("could not apply the policy: {error}");
        return ExitCode::FAILURE;
    }
    println!("policy applied: only {} is reachable", inside.display());

    let still_permitted = std::fs::read(&permitted).is_ok();
    let still_forbidden = std::fs::read(forbidden).is_ok();
    println!(
        "after:  {} readable = {still_permitted}",
        permitted.display()
    );
    println!(
        "after:  {} readable = {still_forbidden}",
        forbidden.display()
    );

    // Cleanup happens inside the sandbox, which is itself a check that the grant works.
    let _ = std::fs::remove_dir_all(&inside);

    if still_permitted && !still_forbidden {
        println!("\nconfinement works: the granted path is reachable and nothing else is");
        ExitCode::SUCCESS
    } else {
        eprintln!("\nCONFINEMENT DID NOT WORK AS INTENDED");
        ExitCode::FAILURE
    }
}
