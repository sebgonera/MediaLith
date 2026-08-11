//! The console's own certificate, and the identity behind it (ADR-0014).
//!
//! ADR-0014 deferred TLS on the grounds that closing the console while the update path was
//! unsigned would protect the smaller opening. That reason is gone: ADR-0006 is finished
//! and proven on hardware, which leaves a root shell and a root-equivalent device token
//! travelling in clear on the LAN as the widest thing still open.
//!
//! # There is no certificate authority, and there cannot be one
//!
//! The appliance has no domain name — its address comes from DHCP and moves — and nobody
//! is going to buy it a certificate. So it issues its own, and the browser will say so the
//! first time. That warning is not noise to be suppressed; it is the only moment a person
//! can check they are talking to their own machine, and ADR-0014 said plainly that the
//! tension between "check the fingerprint on the attached screen" and "this console exists
//! so you never need the attached screen" was real and unresolved. It still is. What this
//! does is make the check *possible*: the fingerprint is printed on the screen at boot and
//! served at `/api/status`, so somebody who wants to verify can, and somebody who does not
//! is still protected from anyone merely listening.
//!
//! # The key outlives the certificate, deliberately
//!
//! The certificate names the addresses the machine currently has, so a DHCP lease that
//! moves means a new certificate. The **key** is generated once and kept, and the
//! fingerprint reported everywhere is the fingerprint of the key rather than of the
//! certificate. A fingerprint that changed whenever the router handed out a different
//! address would teach exactly one lesson: that the warning means nothing.
//!
//! # What has run
//!
//! **Nothing on hardware.** The handshake is exercised against a real client on the build
//! host; no appliance has yet served HTTPS.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::Digest as _;

/// The key, in PKCS#8 DER. Never leaves `/var`.
const KEY_FILE: &str = "key.der";
/// The certificate, in DER.
const CERT_FILE: &str = "cert.der";
/// What the certificate was issued for, one name per line.
///
/// Kept beside the certificate rather than read back out of it. Parsing X.509 to answer
/// "does this still name my address" is a lot of surface for a question a text file
/// answers exactly, and a wrong answer here only ever costs a regenerated certificate.
const NAMES_FILE: &str = "issued-for";

/// A console's TLS identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The certificate, DER.
    pub certificate: Vec<u8>,
    /// The private key, PKCS#8 DER.
    pub key: Vec<u8>,
    /// SHA-256 of the public key, grouped for reading aloud.
    pub fingerprint: String,
    /// Whether this run generated a new key, rather than reusing one.
    ///
    /// Reported because it is the one event that changes what a browser sees. A machine
    /// whose key was replaced will warn again at somebody who has already checked it, and
    /// "it did that again for no reason" is how a security warning stops being read.
    pub key_is_new: bool,
}

/// Loads the identity from `dir`, creating or reissuing what is missing.
///
/// `names` are the addresses and host names the certificate should cover. When they differ
/// from what the stored certificate was issued for, the certificate is reissued **and the
/// key is kept**, so the fingerprint does not move.
///
/// # Errors
/// If the directory cannot be written, or key material cannot be generated. A console that
/// cannot obtain a certificate cannot serve, which is fatal by the choice recorded in
/// ADR-0014's revision: the console listens on HTTPS only.
pub fn load_or_create(dir: &Path, names: &[String]) -> io::Result<Identity> {
    std::fs::create_dir_all(dir)?;

    let key_path = dir.join(KEY_FILE);
    let stored_key = std::fs::read(&key_path).ok();
    let key_is_new = stored_key.is_none();

    let key_pair = match &stored_key {
        Some(der) => rcgen::KeyPair::try_from(der.as_slice()).map_err(bad_material)?,
        None => rcgen::KeyPair::generate().map_err(bad_material)?,
    };

    if key_is_new {
        write_private(&key_path, &key_pair.serialize_der())?;
    }

    let fingerprint = fingerprint_of(&key_pair.public_key_der());
    let certificate = match reusable_certificate(dir, names) {
        Some(certificate) => certificate,
        None => issue(dir, names, &key_pair)?,
    };

    Ok(Identity {
        certificate,
        key: key_pair.serialize_der(),
        fingerprint,
        key_is_new,
    })
}

/// The stored certificate, if it was issued for exactly these names.
fn reusable_certificate(dir: &Path, names: &[String]) -> Option<Vec<u8>> {
    let issued_for = std::fs::read_to_string(dir.join(NAMES_FILE)).ok()?;
    (issued_for.lines().collect::<Vec<_>>() == names)
        .then(|| std::fs::read(dir.join(CERT_FILE)).ok())?
}

/// Issues a certificate for `names` under an existing key.
fn issue(dir: &Path, names: &[String], key_pair: &rcgen::KeyPair) -> io::Result<Vec<u8>> {
    let mut params = rcgen::CertificateParams::new(names.to_vec()).map_err(bad_material)?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "MediaLith console");

    let certificate = params.self_signed(key_pair).map_err(bad_material)?;
    let der = certificate.der().to_vec();

    std::fs::write(dir.join(CERT_FILE), &der)?;
    std::fs::write(dir.join(NAMES_FILE), names.join("\n"))?;
    Ok(der)
}

/// SHA-256 of a public key, in the shape a person can read out.
///
/// Uppercase hex in pairs. This is what somebody compares against the screen, so it is
/// formatted for a human reading aloud rather than for a machine parsing it — the format
/// browsers show for a certificate's SHA-256, so the two can be laid side by side.
#[must_use]
pub fn fingerprint_of(public_key_der: &[u8]) -> String {
    let digest = sha2::Sha256::digest(public_key_der);
    digest
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The rustls configuration for this identity.
///
/// # Errors
/// If rustls refuses the key or certificate, which for material this module just produced
/// means a version mismatch rather than a bad file.
pub fn server_config(identity: &Identity) -> io::Result<Arc<rustls::ServerConfig>> {
    let certificate = rustls::pki_types::CertificateDer::from(identity.certificate.clone());
    let key = rustls::pki_types::PrivateKeyDer::try_from(identity.key.clone())
        .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why.to_string()))?;

    // No client authentication. The device token is the credential (ADR-0013); TLS is here
    // to stop it being read off the wire, not to replace it.
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .map_err(bad_material)?;

    Ok(Arc::new(config))
}

/// The names a certificate for this machine should cover.
///
/// Every address it has, plus loopback and the host name. The addresses are what somebody
/// types into a browser; loopback is what the machine's own probes use.
#[must_use]
pub fn names_for(addresses: &[String], hostname: &str) -> Vec<String> {
    let mut names: Vec<String> = addresses.to_vec();
    for extra in ["127.0.0.1", "localhost"] {
        names.push(extra.to_owned());
    }
    if !hostname.is_empty() && !names.iter().any(|n| n == hostname) {
        names.push(hostname.to_owned());
    }
    // Sorted and deduplicated so that the same machine produces the same list in the same
    // order across boots. Without it, an address arriving in a different order would look
    // like a changed name set and reissue a certificate every time.
    names.sort();
    names.dedup();
    names
}

/// The fingerprint this console is serving, once it has one.
static FINGERPRINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Records the fingerprint so the status report can carry it.
///
/// A `OnceLock` rather than a value threaded through every handler, for the same reason
/// [`crate::auth`] does it: the identity is a property of the machine, not of a request,
/// and every route that wanted it would otherwise grow a parameter it does not use.
pub fn remember(fingerprint: &str) {
    let _ = FINGERPRINT.set(fingerprint.to_owned());
}

/// The fingerprint of the key this console serves, if it is serving TLS.
#[must_use]
pub fn fingerprint() -> Option<String> {
    FINGERPRINT.get().cloned()
}

/// Writes key material with permissions set before any bytes reach the disk.
fn write_private(path: &Path, der: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    // mode() on create rather than a chmod afterwards, for the reason `plexos-sign` gives
    // about root keys: between a default-mode create and a later chmod there is a window
    // in which the world can read it.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(der)?;
    file.sync_all()
}

/// Turns a key or certificate failure into an `io::Error` that names the file it is about.
fn bad_material(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "the console's TLS identity could not be used: {error}. Remedy: delete {} and \
             restart -- a new key will be generated, and the browser will warn once more \
             because the fingerprint has changed.",
            PathBuf::from(plexos_types::paths::TLS_DIR).display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plexos-tls-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn names() -> Vec<String> {
        vec!["127.0.0.1".to_owned(), "192.168.2.102".to_owned()]
    }

    #[test]
    fn an_identity_is_created_and_then_reused_unchanged() {
        // The property the whole design rests on. A fingerprint that moved on its own
        // would train somebody to click through the one warning that means anything.
        let dir = scratch("reuse");

        let first = load_or_create(&dir, &names()).expect("creates");
        assert!(first.key_is_new);

        let second = load_or_create(&dir, &names()).expect("reuses");
        assert!(!second.key_is_new, "the key must not be regenerated");
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.key, second.key);
        assert_eq!(
            first.certificate, second.certificate,
            "an unchanged name set must not reissue"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_address_reissues_the_certificate_and_keeps_the_fingerprint() {
        // DHCP moves. The certificate has to follow the address and the fingerprint must
        // not, or every lease renewal looks like an attack.
        let dir = scratch("moved");
        let first = load_or_create(&dir, &names()).expect("creates");

        let moved = vec!["127.0.0.1".to_owned(), "192.168.2.200".to_owned()];
        let second = load_or_create(&dir, &moved).expect("reissues");

        assert_ne!(
            first.certificate, second.certificate,
            "the certificate must cover the address somebody types"
        );
        assert_eq!(
            first.fingerprint, second.fingerprint,
            "the fingerprint is the key's, and the key did not change"
        );
        assert!(!second.key_is_new);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("mode");
        load_or_create(&dir, &names()).expect("creates");

        let mode = std::fs::metadata(dir.join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a readable console key is not a console key");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fingerprint_is_shaped_for_somebody_reading_it_off_a_screen() {
        // It is compared by eye against what a browser shows, which is uppercase hex in
        // colon-separated pairs. A different shape is a comparison nobody completes.
        let fingerprint = fingerprint_of(b"any key material");
        assert_eq!(fingerprint.len(), 32 * 3 - 1, "{fingerprint}");
        assert_eq!(fingerprint.matches(':').count(), 31);
        assert!(
            fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_lowercase() || c == ':'),
            "{fingerprint}"
        );
    }

    #[test]
    fn the_name_list_is_stable_across_boots() {
        // Otherwise an address arriving in a different order reads as a changed name set
        // and reissues a certificate on every boot.
        let one = names_for(
            &["192.168.2.102".to_owned(), "10.0.0.5".to_owned()],
            "plexos",
        );
        let other = names_for(
            &["10.0.0.5".to_owned(), "192.168.2.102".to_owned()],
            "plexos",
        );
        assert_eq!(one, other);
        assert!(one.contains(&"127.0.0.1".to_owned()));
        assert!(one.contains(&"plexos".to_owned()));

        // And a machine with no address at all still gets a usable certificate, because
        // the console has to be able to say why it is unreachable.
        assert!(!names_for(&[], "").is_empty());
    }

    #[test]
    fn rustls_accepts_what_this_module_produces() {
        // The signer-and-verifier problem again, in a different costume: material that
        // looks right and that the TLS stack refuses is a console that will not start, on
        // a machine whose console is how you would find that out.
        let dir = scratch("rustls");
        let identity = load_or_create(&dir, &names()).expect("creates");
        server_config(&identity).expect("rustls accepts it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs `openssl x509` over a certificate, or `None` when there is no openssl here.
    ///
    /// `name` makes the scratch file unique per test. Tests run as threads in one process
    /// and a shared path is a race: one test deletes the file the other is reading, and
    /// the pair fails in whichever order the scheduler picked. Found by writing it that
    /// way first.
    fn inspect(name: &str, der: &[u8], args: &[&str]) -> Option<String> {
        let file = std::env::temp_dir().join(format!("plexos-tls-inspect-{name}.der"));
        std::fs::write(&file, der).ok()?;
        let out = std::process::Command::new("openssl")
            .args(["x509", "-inform", "DER", "-noout", "-in"])
            .arg(&file)
            .args(args)
            .output()
            .ok()?;
        let _ = std::fs::remove_file(&file);
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    #[test]
    fn an_address_is_an_ip_san_and_not_a_name() {
        // Browsers will not accept a certificate for https://192.168.2.102/ unless the
        // address is an *IP* SAN; the same string as a DNS entry is ignored. Getting this
        // wrong produces a certificate that looks correct in every field a person would
        // read and that no browser will take -- on the one change where the cost of being
        // wrong is the console itself.
        let dir = scratch("sans");
        let identity =
            load_or_create(&dir, &names_for(&["192.168.2.102".to_owned()], "plexos")).unwrap();

        let Some(sans) = inspect("sans", &identity.certificate, &["-ext", "subjectAltName"]) else {
            println!("skip: no openssl on this host, so the SAN types were not checked");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };

        assert!(sans.contains("IP Address:192.168.2.102"), "{sans}");
        assert!(sans.contains("IP Address:127.0.0.1"), "{sans}");
        assert!(sans.contains("DNS:plexos"), "{sans}");
        assert!(
            !sans.contains("DNS:192.168.2.102"),
            "an address recorded as a name is one no browser will match: {sans}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_certificate_does_not_depend_on_the_clock_being_right() {
        // This appliance has an RTC and no time synchronisation, and plexos_update::clock
        // exists because its clock can be arbitrarily wrong. A certificate valid from
        // "now" would be *not yet valid* on a machine whose battery died, which a browser
        // refuses exactly as hard as an expired one -- and the console is how you would
        // have found out.
        let dir = scratch("dates");
        let identity = load_or_create(&dir, &names()).unwrap();

        let Some(dates) = inspect("dates", &identity.certificate, &["-dates"]) else {
            println!("skip: no openssl on this host, so the validity was not checked");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };

        // Parsed loosely on purpose: what matters is that the window is absurdly wide,
        // not which particular years rcgen picked.
        let before = dates
            .lines()
            .find_map(|l| l.strip_prefix("notBefore="))
            .expect("a notBefore");
        let after = dates
            .lines()
            .find_map(|l| l.strip_prefix("notAfter="))
            .expect("a notAfter");
        let year = |line: &str| -> i32 {
            line.split_whitespace()
                .nth(3)
                .and_then(|y| y.parse().ok())
                .unwrap_or(0)
        };

        assert!(year(before) <= 2000, "valid from {before}");
        assert!(year(after) >= 2100, "valid until {after}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
