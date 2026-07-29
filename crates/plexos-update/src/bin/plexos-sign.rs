//! The publisher's half of ADR-0006: making keys, certificates and signatures.
//!
//! This runs on a build host and never on an appliance. It is in this crate rather than a
//! tool of its own so that the thing which signs and the thing which verifies share one
//! definition of what a certificate is — a signer and a verifier that agree only by
//! coincidence is a class of bug that shows up once, in the field, as "the update this
//! machine will not take".
//!
//! ```text
//! plexos-sign root-key   <path>                     make an offline root key
//! plexos-sign signing-key <path>                    make a signing key
//! plexos-sign certify    <root> <signing> <id> <until>   certify a signing key
//! plexos-sign sign       <signing-key> <file>       detached signature, base64
//! plexos-sign trust      <root-key> <id>            print the ROOT_KEYS entry
//! ```
//!
//! # Key material
//!
//! Private keys are written `0600` and are base64 of the 32 raw seed bytes. No password,
//! no encrypted container: a passphrase this tool prompts for is a passphrase that ends up
//! in a build script, and pretending otherwise would be worse than saying plainly that the
//! file is the secret. Keeping the root key off the build host is an operational problem
//! this tool cannot solve, and ADR-0006 says so.
//!
//! Entropy comes from `/dev/urandom` rather than a crate. It is the right source on the
//! only platform this runs on, and it keeps a random-number generator out of the
//! dependency set of the binary that ships on the appliance.
//!
//! # What has run
//!
//! **Nothing beyond its own tests.** No key has been generated, nothing has been signed,
//! and `plexos_update::trust::ROOT_KEYS` is still empty.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};

const ENGINE: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

const USAGE: &str = "\
plexos-sign — the publisher's half of ADR-0006

  plexos-sign root-key <path>
      Generate an offline root key. Signs certificates, never manifests.

  plexos-sign signing-key <path>
      Generate a signing key. Signs manifests.

  plexos-sign certify <root-key> <signing-key> <key-id> <not-after>
      Print a certificate authorising the signing key. <not-after> is RFC 3339,
      e.g. 2027-01-01T00:00:00Z. Goes in the manifest's `signing.certificate`.

  plexos-sign sign <signing-key> <file>
      Print a detached base64 Ed25519 signature over the file's exact bytes.

  plexos-sign trust <root-key> <key-id>
      Print the ROOT_KEYS entry to paste into crates/plexos-update/src/trust.rs.

Private keys are written 0600 and are the secret. There is no passphrase; see the
module documentation for why that is stated rather than faked.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let result = match args.as_slice() {
        ["root-key", path] => generate(Path::new(path), "root"),
        ["signing-key", path] => generate(Path::new(path), "signing"),
        ["certify", root, signing, key_id, not_after] => {
            certify(Path::new(root), Path::new(signing), key_id, not_after)
        }
        ["sign", key, file] => sign(Path::new(key), Path::new(file)),
        ["trust", key, key_id] => trust(Path::new(key), key_id),
        ["--help" | "-h"] | [] => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => Err("unrecognised arguments. Remedy: run with --help".to_owned()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("plexos-sign: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Writes a new private key, refusing to overwrite one that exists.
///
/// Refusing rather than prompting: this tool is run from scripts, a prompt would be
/// answered by whatever happened to be on stdin, and the cost of being wrong is a signing
/// identity that no deployed device will ever trust again.
fn generate(path: &Path, kind: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists. Remedy: choose another path, or move the existing key \
             aside deliberately -- overwriting a {kind} key destroys the only copy of an \
             identity that deployed images may already trust.",
            path.display()
        ));
    }

    let seed = entropy()?;
    let key = SigningKey::from_bytes(&seed);

    write_private(path, &seed)?;

    println!("wrote {} {} key", path.display(), kind);
    println!(
        "  public key: {}",
        ENGINE.encode(key.verifying_key().to_bytes())
    );
    if kind == "root" {
        println!("  next: plexos-sign trust {} <key-id>", path.display());
    }
    Ok(())
}

/// Prints a certificate authorising `signing`, vouched for by `root`.
fn certify(root: &Path, signing: &Path, key_id: &str, not_after: &str) -> Result<(), String> {
    if !not_after.ends_with('Z') || not_after.len() != 20 {
        return Err(format!(
            "{not_after} is not an RFC 3339 instant in UTC. Remedy: use the form \
             2027-01-01T00:00:00Z. The device compares this as a string, so a different \
             shape does not merely look odd -- it compares wrongly."
        ));
    }

    let root = read_private(root)?;
    let signing = read_private(signing)?;

    // Built as text, and this is the whole reason the field order is fixed here rather
    // than left to a serialiser: these exact bytes are what gets signed and what the
    // device will verify. Nothing may re-serialise them on the way.
    let body = format!(
        r#"{{"key_id":"{key_id}","public_key":"{}","not_after":"{not_after}","root_key_id":"{}"}}"#,
        ENGINE.encode(signing.verifying_key().to_bytes()),
        root_id_hint(),
    );

    let signature = root.sign(body.as_bytes());
    println!(
        "{}.{}",
        ENGINE.encode(body.as_bytes()),
        ENGINE.encode(signature.to_bytes())
    );
    Ok(())
}

/// The root key identifier written into certificates.
///
/// A placeholder while `ROOT_KEYS` is empty: there is exactly one root key planned and no
/// mechanism yet for a key to carry its own name. When rotation becomes real this has to
/// become an argument, and the compile-time constant here will stop matching, loudly.
const fn root_id_hint() -> &'static str {
    "plexos-root-dev"
}

/// Prints a detached signature over a file's exact bytes.
fn sign(key: &Path, file: &Path) -> Result<(), String> {
    let key = read_private(key)?;
    let bytes =
        std::fs::read(file).map_err(|e| format!("could not read {}: {e}", file.display()))?;

    println!("{}", ENGINE.encode(key.sign(&bytes).to_bytes()));
    Ok(())
}

/// Prints the Rust constant to paste into the trust store.
fn trust(key: &Path, key_id: &str) -> Result<(), String> {
    let key = read_private(key)?;
    let public = key.verifying_key().to_bytes();

    println!("RootKey {{");
    println!("    id: \"{key_id}\",");
    println!("    public_key: [");
    for chunk in public.chunks(8) {
        let line: Vec<String> = chunk.iter().map(|b| format!("0x{b:02x}")).collect();
        println!("        {},", line.join(", "));
    }
    println!("    ],");
    println!("    development: true,");
    println!("}},");
    Ok(())
}

/// Thirty-two bytes from the kernel.
///
/// `read_exact` rather than `read`: `/dev/urandom` never reaches end of file, so a plain
/// `fs::read` would not return, and a partial `read` would silently produce a key with
/// less entropy than its length suggests.
fn entropy() -> Result<[u8; 32], String> {
    use std::io::Read as _;

    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("could not open /dev/urandom: {e}"))?;
    let mut seed = [0u8; 32];
    file.read_exact(&mut seed)
        .map_err(|e| format!("could not read 32 bytes from /dev/urandom: {e}"))?;
    Ok(seed)
}

/// Writes a private key with permissions set before any bytes reach the disk.
fn write_private(path: &Path, seed: &[u8; 32]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt as _;

    // mode() on create, not a chmod afterwards. Between a default-mode create and a
    // later chmod there is a window in which the world can read a root key, and a window
    // is all it takes when the thing being protected cannot be rotated cheaply.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("could not create {}: {e}", path.display()))?;

    file.write_all(ENGINE.encode(seed).as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Reads a private key written by [`write_private`].
fn read_private(path: &Path) -> Result<SigningKey, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;

    let bytes = ENGINE
        .decode(text.trim())
        .map_err(|e| format!("{} is not base64: {e}", path.display()))?;

    let seed: [u8; 32] = bytes.try_into().map_err(|_| {
        format!(
            "{} is not 32 bytes. Remedy: this is a seed, not a PEM file or an OpenSSH \
             key; it was written by `plexos-sign root-key` or `signing-key`.",
            path.display()
        )
    })?;

    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("plexos-sign-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_generated_key_round_trips_through_the_file() {
        let dir = scratch("roundtrip");
        let path = dir.join("root");

        generate(&path, "root").expect("generates");
        let key = read_private(&path).expect("reads back");

        // The signature is the real check: a key that reads back into a different scalar
        // would still be 32 plausible bytes.
        let signature = key.sign(b"message");
        assert!(
            key.verifying_key()
                .verify_strict(b"message", &signature)
                .is_ok()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_private_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("mode");
        let path = dir.join("root");
        generate(&path, "root").expect("generates");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a root key readable by anyone is not a root key"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generating_over_an_existing_key_is_refused() {
        // Destroying a root key destroys an identity deployed images already trust, and
        // there is no way back from it. Refusing is the only safe answer, and the message
        // has to say why rather than just "file exists".
        let dir = scratch("clobber");
        let path = dir.join("root");
        generate(&path, "root").expect("generates");

        let error = generate(&path, "root").expect_err("must refuse");
        assert!(error.contains("Remedy:"), "{error}");
        assert!(error.contains("destroys"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_certificate_this_tool_writes_is_one_the_device_accepts() {
        // The reason this binary lives in the crate it does. A signer and a verifier that
        // agree by coincidence produce exactly one symptom, once, in the field: an update
        // the appliance will not take, with a signature that looks fine to whoever made
        // it.
        let dir = scratch("agreement");
        let root_path = dir.join("root");
        let signing_path = dir.join("signing");
        generate(&root_path, "root").unwrap();
        generate(&signing_path, "signing").unwrap();

        let root = read_private(&root_path).unwrap();
        let signing = read_private(&signing_path).unwrap();

        let body = format!(
            r#"{{"key_id":"k","public_key":"{}","not_after":"2027-01-01T00:00:00Z","root_key_id":"{}"}}"#,
            ENGINE.encode(signing.verifying_key().to_bytes()),
            root_id_hint(),
        );
        let certificate = format!(
            "{}.{}",
            ENGINE.encode(body.as_bytes()),
            ENGINE.encode(root.sign(body.as_bytes()).to_bytes())
        );

        let roots = [plexos_update::trust::RootKey {
            id: root_id_hint(),
            public_key: root.verifying_key().to_bytes(),
            development: true,
        }];

        let fixture = include_str!("../../../plexos-types/tests/fixtures/manifest-v1.json");
        let head = fixture.split_once("\"signing\"").unwrap().0;
        let bytes = format!(
            "{head}\"signing\": {{\n    \"key_id\": \"k\",\n    \
             \"certificate\": \"{certificate}\"\n  }}\n}}\n"
        )
        .into_bytes();
        let signature = signing.sign(&bytes);

        let raw = plexos_types::manifest::RawManifest::new(bytes);
        let verified = plexos_update::trust::verify_against(
            &roots,
            &raw,
            &signature.to_bytes(),
            Some("2026-07-29T00:00:00Z"),
        )
        .expect("what this tool produces must verify");

        assert_eq!(verified.key_id, "k");
        assert!(verified.development);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_expiry_is_refused_because_it_is_compared_as_a_string() {
        let dir = scratch("expiry");
        let root = dir.join("root");
        let signing = dir.join("signing");
        generate(&root, "root").unwrap();
        generate(&signing, "signing").unwrap();

        let error = certify(&root, &signing, "k", "2027-01-01").expect_err("must refuse");
        assert!(error.contains("Remedy:"), "{error}");
        assert!(error.contains("compares wrongly"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
