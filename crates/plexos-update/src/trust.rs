//! Deciding whether a manifest may be believed (ADR-0006).
//!
//! Until this module existed, the updater trusted whoever answered at the address it was
//! given. That is acceptable on a bench and nowhere else, and it was the widest gap
//! between what the appliance does and what it should be allowed to do.
//!
//! # The shape of the trust
//!
//! Two tiers, because a signing key that can never be replaced is a signing key that can
//! never be compromised safely.
//!
//! - **Root keys** are compiled into this crate and therefore live in `/usr`, covered by
//!   the verity root hash and — once images are signed — by the UKI signature (ADR-0004).
//!   They sign nothing but certificates, and they change only when the OS does.
//! - **Signing keys** sign manifests. Each carries a certificate, signed by a root key,
//!   which travels *with* the manifest. That is what makes rotation possible: a device
//!   needs no prior knowledge of the current signing key.
//!
//! # Signatures cover bytes
//!
//! Every signature here is over a literal byte string that was received, never over
//! something re-serialised from a parsed structure. JSON canonicalisation is a well-worn
//! source of signature-bypass bugs and the only reliable defence is to never depend on
//! it. [`plexos_types::manifest::RawManifest`] holds the manifest bytes for that reason,
//! and a certificate carries its own signed bytes rather than being reconstructed from
//! its fields.
//!
//! # Certificate encoding
//!
//! `base64(body) "." base64(signature)`, where `body` is the exact JSON that was signed.
//! Two fields, one separator, and the bytes that were signed are recoverable without
//! re-encoding anything.
//!
//! It resembles a JWT and deliberately omits the part of JWT that keeps causing
//! CVEs: there is no algorithm field. Ed25519 is the only thing this understands, so
//! there is nothing for an attacker to negotiate down to `none`.
//!
//! # What has run
//!
//! **Nothing on hardware.** The types and their tests are exercised by
//! `cargo test`; no appliance has yet been offered a signed manifest.

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};

use plexos_types::manifest::{Manifest, RawManifest};

/// A root key: the end of the chain, and the only thing believed without proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootKey {
    /// Identifier, matched against a certificate's `root_key_id`.
    pub id: &'static str,
    /// The Ed25519 public key, 32 bytes.
    pub public_key: [u8; 32],
    /// Whether this key is a real root of trust or a development stand-in.
    ///
    /// A development key's private half is on a build host rather than offline, and this
    /// is reported rather than hidden: an appliance that says "signed" while trusting a
    /// key anyone with the repository can find has told the reader something false.
    pub development: bool,
}

/// The root keys this build believes.
///
/// An empty set is **not** a reason to accept anything: with no root key an appliance
/// refuses every update, which is the safe direction and not a usable one.
///
/// The one entry here is a **development** key, and everything about how it is held is
/// weaker than ADR-0006 asks for: its private half is a file on a build host, not an
/// offline secret, so what a signature proves today is "this came from that build host".
/// That is a large improvement on "this came from whoever answered at the address", which
/// is what preceded it, and it is not the same as a root of trust. `development: true` is
/// carried out of verification into [`Verified::development`] and onto the console page so
/// that nothing anywhere says "signed" without saying signed by what.
///
/// Replacing it with a real one is not an edit to this constant alone: root keys change
/// only with the image that carries them, so every deployed appliance has to take an
/// update signed by the *old* key before it will believe the new one. Add the new key
/// beside this one, ship that, and remove this one in a later release.
///
/// `plexos-sign root-key` writes the private half and `plexos-sign trust` prints the
/// constant to paste here. The private half never enters the repository.
pub const ROOT_KEYS: &[RootKey] = &[RootKey {
    id: "plexos-root-dev",
    public_key: [
        0xc6, 0xfe, 0x18, 0x05, 0x9c, 0x19, 0x6b, 0x96, 0x0c, 0x4a, 0x35, 0xaa, 0xe8, 0xce, 0x87,
        0xde, 0x2f, 0x28, 0xb0, 0xbb, 0x01, 0x4d, 0x2b, 0x5d, 0x08, 0x75, 0x5e, 0x78, 0xb9, 0x65,
        0x39, 0x5c,
    ],
    development: true,
}];

/// Everything that decides whether a manifest may be believed.
///
/// One parameter rather than four, because these are not independent settings: they are
/// the trust policy, and a caller that could pass three of them is a caller that can forget
/// the fourth. The one most likely to be forgotten is [`Policy::revoked`], which is empty
/// in the safe-looking direction.
#[derive(Debug, Clone, Copy)]
pub struct Policy<'a> {
    /// The keys believed without proof.
    pub roots: &'a [RootKey],
    /// Signing key identifiers that must no longer be believed.
    pub revoked: &'a [String],
    /// The device's idea of the current time, RFC 3339, or `None` if it cannot tell.
    ///
    /// `None` skips the expiry check, and that is a decision the caller makes rather than
    /// an accident: this appliance has no time synchronisation, and a wrong clock that is
    /// believed refuses every future update. See [`expiry_is_checkable`].
    pub now: Option<&'a str>,
}

impl<'a> Policy<'a> {
    /// The policy of this image: its compiled-in roots, nothing revoked.
    #[must_use]
    pub const fn of_this_build(now: Option<&'a str>) -> Self {
        Self {
            roots: ROOT_KEYS,
            revoked: &[],
            now,
        }
    }

    /// The same policy, with a revocation list applied.
    #[must_use]
    pub fn revoking(self, revoked: &'a [String]) -> Self {
        Self { revoked, ..self }
    }
}

/// Why a manifest was not believed.
///
/// Every variant names what to do about it, because a refusal a person cannot act on
/// stops an update and explains nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustError {
    /// This build has no root keys at all, so nothing can be verified.
    NoRootKeys,
    /// The manifest itself could not be read, so there is no certificate to find.
    ///
    /// Separate from the certificate errors, and it took a test to notice why: a
    /// manifest from a future `manifest_version` is a perfectly ordinary thing for an old
    /// device to be offered, and reporting it as a broken certificate would send someone
    /// to look at the signing setup instead of at the version they published.
    Unparsable(String),
    /// The certificate was not two base64 fields separated by a dot.
    MalformedCertificate(String),
    /// The certificate names a root key this build does not have.
    UnknownRootKey {
        /// The `root_key_id` the certificate asked for.
        wanted: String,
        /// The identifiers this build does have.
        known: Vec<String>,
    },
    /// The certificate's signature does not verify against the root key it names.
    CertificateNotSigned(String),
    /// The certificate has expired, and the clock is trustworthy enough to say so.
    CertificateExpired {
        /// When the certificate stopped being valid.
        not_after: String,
        /// What the device thinks the time is.
        now: String,
    },
    /// The manifest's `key_id` is not the one the certificate authorises.
    KeyIdMismatch {
        /// What the manifest claimed.
        manifest: String,
        /// What the certificate actually authorises.
        certificate: String,
    },
    /// The signing key is on a revocation list this device holds.
    KeyRevoked {
        /// The key the manifest was signed with.
        key_id: String,
    },
    /// The detached signature does not verify against the certified signing key.
    ManifestNotSigned(String),
    /// A key or signature was not a well-formed Ed25519 value.
    BadKeyMaterial(String),
    /// A revocation list could not be read, or was not signed by a root key.
    MalformedRevocations(String),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRootKeys => write!(
                f,
                "this build has no root keys compiled in, so no manifest can be \
                 verified. Remedy: generate one with `plexos-sign root-key`, paste what \
                 `plexos-sign trust` prints into ROOT_KEYS, and rebuild the image. An \
                 appliance with no root keys refuses every update rather than accepting \
                 any, which is the safe direction and not a usable one."
            ),
            Self::Unparsable(why) => write!(
                f,
                "this manifest could not be read at all: {why}. Remedy: check what was \
                 published. The commonest innocent cause is a manifest_version newer \
                 than this image understands, which means the appliance needs an update \
                 delivered another way before it can take this one."
            ),
            Self::MalformedCertificate(why) => write!(
                f,
                "the certificate in this manifest is not readable: {why}. Remedy: it \
                 must be base64(body).base64(signature); re-sign the manifest with \
                 plexos-sign, and check that nothing reformatted the file in transit."
            ),
            Self::UnknownRootKey { wanted, known } => write!(
                f,
                "this manifest was certified by root key {wanted}, which this image does \
                 not trust. It trusts: {}. Remedy: either the publisher signed with the \
                 wrong root key, or this appliance predates a root key rotation and \
                 needs an OS update delivered another way -- root keys change only with \
                 the image that carries them.",
                if known.is_empty() {
                    "nothing".to_owned()
                } else {
                    known.join(", ")
                }
            ),
            Self::CertificateNotSigned(why) => write!(
                f,
                "the signing certificate is not validly signed by the root key it names \
                 ({why}). Remedy: treat this bundle as hostile. A certificate that names \
                 a real root key and does not verify against it is not a mistake that \
                 happens by accident."
            ),
            Self::CertificateExpired { not_after, now } => write!(
                f,
                "the signing certificate expired at {not_after} and this device believes \
                 it is now {now}. Remedy: publish with a current signing key. If the date \
                 above is wrong, the appliance's clock is wrong -- check it before \
                 blaming the bundle."
            ),
            Self::KeyIdMismatch {
                manifest,
                certificate,
            } => write!(
                f,
                "the manifest says it was signed by {manifest} but its certificate \
                 authorises {certificate}. Remedy: treat this bundle as hostile; the two \
                 halves of it came from different places."
            ),
            Self::KeyRevoked { key_id } => write!(
                f,
                "this manifest was signed by {key_id}, which has been revoked. Remedy: \
                 this update must not be installed even though its signature is valid -- \
                 that is what revoking a key means. Publish with the current signing key. \
                 If you believe the revocation is the mistake, it can only be undone by a \
                 root-signed list with a higher counter, or by an OS update; there is \
                 deliberately no way to withdraw one from the network."
            ),
            Self::MalformedRevocations(why) => write!(
                f,
                "the revocation list could not be used: {why}. Remedy: the list this \
                 appliance already holds is unchanged and still in force, so nothing has \
                 become more permissive. Re-publish the list with plexos-sign revoke."
            ),
            Self::ManifestNotSigned(why) => write!(
                f,
                "the manifest's signature does not verify against the key its certificate \
                 authorises ({why}). Remedy: treat this bundle as hostile. If the \
                 publisher is trusted, the likely innocent cause is a tool that \
                 reformatted the manifest after signing -- the signature covers exact \
                 bytes, so even reindenting it breaks this."
            ),
            Self::BadKeyMaterial(why) => write!(
                f,
                "a key or signature in this bundle is not a well-formed Ed25519 value: \
                 {why}. Remedy: re-run plexos-sign; this is a malformed artefact rather \
                 than a wrong one."
            ),
        }
    }
}

impl std::error::Error for TrustError {}

/// What a certificate says, once its signature has been checked.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CertificateBody {
    /// Identifier of the key this certificate authorises.
    pub key_id: String,
    /// The authorised Ed25519 signing key, base64.
    pub public_key: String,
    /// RFC 3339 instant after which this certificate must not be believed.
    pub not_after: String,
    /// Which root key vouches for this.
    pub root_key_id: String,
}

/// A manifest whose whole chain has been checked.
///
/// Constructed only by [`verify`], so holding one is proof rather than an assertion. That
/// is the point of the type: an updater that took a `Manifest` and a `bool` would be one
/// forgotten check away from installing anything.
#[derive(Debug, Clone)]
pub struct Verified {
    /// The manifest itself.
    pub manifest: Manifest,
    /// Which signing key vouched for it.
    pub key_id: String,
    /// Which root key vouched for that.
    pub root_key_id: String,
    /// Whether the root of this chain was a development key.
    ///
    /// Carried out of the verification rather than looked up later, so that whatever
    /// reports "this update is signed" is holding the answer to "signed by what".
    pub development: bool,
}

/// Verifies a manifest, its detached signature, and the certificate chain behind it.
///
/// The policy is a parameter, including the root keys. That is partly so the accept path
/// can be tested at all — with only the [`ROOT_KEYS`] constant to work from, every test
/// would run against an empty trust store, exercising the refusal and nothing else, and a
/// verifier that accepted everything would pass the suite. It is also the honest way to
/// say what a set of root keys is: a parameter of the decision, not a property of the
/// universe.
///
/// # Errors
/// Any break in the chain, each naming what to do about it.
pub fn verify(
    policy: &Policy<'_>,
    raw: &RawManifest,
    signature: &[u8],
) -> Result<Verified, TrustError> {
    let roots = policy.roots;
    if roots.is_empty() {
        return Err(TrustError::NoRootKeys);
    }

    // Parsed before anything is trusted, because the certificate lives inside it. Nothing
    // read here is believed until the signature below verifies over the raw bytes -- the
    // parse is a way to find the certificate, not a source of truth.
    let manifest = raw
        .parse()
        .map_err(|e| TrustError::Unparsable(e.to_string()))?;

    let (body, cert_signature) = split_certificate(&manifest.signing.certificate)?;
    let certificate: CertificateBody = serde_json::from_slice(&body)
        .map_err(|e| TrustError::MalformedCertificate(e.to_string()))?;

    let root = roots
        .iter()
        .find(|k| k.id == certificate.root_key_id)
        .ok_or_else(|| TrustError::UnknownRootKey {
            wanted: certificate.root_key_id.clone(),
            known: roots.iter().map(|k| k.id.to_owned()).collect(),
        })?;

    // Over `body` exactly as it arrived, not over a re-serialisation of `certificate`.
    let root_key = VerifyingKey::from_bytes(&root.public_key)
        .map_err(|e| TrustError::BadKeyMaterial(e.to_string()))?;
    verify_detached(&root_key, &body, &cert_signature).map_err(TrustError::CertificateNotSigned)?;

    if let Some(now) = policy.now
        && now > certificate.not_after.as_str()
    {
        return Err(TrustError::CertificateExpired {
            not_after: certificate.not_after.clone(),
            now: now.to_owned(),
        });
    }

    if manifest.signing.key_id != certificate.key_id {
        return Err(TrustError::KeyIdMismatch {
            manifest: manifest.signing.key_id.clone(),
            certificate: certificate.key_id.clone(),
        });
    }

    // After the certificate has been checked and before the manifest signature is. The
    // order is what makes the answer mean anything: revoking is about the key the root
    // actually certified, not about whatever the manifest claimed, and a revoked key must
    // be refused whether or not it signed this document correctly.
    if policy.revoked.contains(&certificate.key_id) {
        return Err(TrustError::KeyRevoked {
            key_id: certificate.key_id,
        });
    }

    let signing_key = decode_key(&certificate.public_key)?;
    verify_detached(&signing_key, raw.signed_bytes(), signature)
        .map_err(TrustError::ManifestNotSigned)?;

    Ok(Verified {
        key_id: certificate.key_id,
        root_key_id: certificate.root_key_id,
        development: root.development,
        manifest,
    })
}

/// Whether a device's clock is plausible enough to judge certificate expiry.
///
/// This appliance has an RTC and no time synchronisation, so its clock can be arbitrarily
/// wrong — and a wrong clock that is *believed* refuses valid updates forever, which is
/// indistinguishable from a bricked update path. A clock reading earlier than the image's
/// own build date is definitely wrong, because the image cannot predate itself.
///
/// The asymmetry is deliberate. Skipping the expiry check costs the narrow protection of
/// expiry, which matters only in combination with a key compromise the revocation list
/// also covers. Enforcing it against a dead RTC costs every future update.
#[must_use]
pub fn expiry_is_checkable(now: &str, image_built_at: &str) -> bool {
    now >= image_built_at
}

/// A root-signed list of signing keys that must no longer be believed.
///
/// Expiry alone cannot handle a compromised key: the certificate an attacker holds is
/// valid until it expires, and shortening every certificate's life to shorten that window
/// means a publisher who forgets to re-certify bricks the update path.
///
/// The counter is what stops the list itself being rolled back. A device keeps the highest
/// it has seen, so serving an older list — genuinely root-signed, from before the
/// revocation — un-revokes nothing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Revocations {
    /// Monotonic counter. A list may only be replaced by one carrying a higher value.
    pub counter: u64,
    /// Which root key vouches for this list.
    pub root_key_id: String,
    /// The signing key identifiers that are no longer believed.
    pub revoked: Vec<String>,
}

impl Revocations {
    /// An empty list, which revokes nothing and is superseded by any signed one.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            counter: 0,
            root_key_id: String::new(),
            revoked: Vec::new(),
        }
    }

    /// Whether this list may replace `held`.
    ///
    /// Strictly higher. Equal counters with different contents means somebody published
    /// two lists with one number, and the safe reading is to keep what is already in
    /// force — a replacement can only ever remove entries, so "keep the old one" is the
    /// direction that revokes more rather than less.
    #[must_use]
    pub fn supersedes(&self, held: &Self) -> bool {
        self.counter > held.counter
    }
}

/// Checks a revocation list against the root keys, and returns what it says.
///
/// Same encoding as a certificate — `base64(body).base64(signature)` — and same reason:
/// the bytes that were signed are recoverable without re-encoding anything.
///
/// # Errors
/// [`TrustError::MalformedRevocations`] for anything unreadable or not properly signed,
/// and [`TrustError::UnknownRootKey`] when it names a root this image does not have.
pub fn verify_revocations(roots: &[RootKey], document: &str) -> Result<Revocations, TrustError> {
    if roots.is_empty() {
        return Err(TrustError::NoRootKeys);
    }

    let (body, signature) =
        split_signed(document.trim()).map_err(TrustError::MalformedRevocations)?;
    let list: Revocations = serde_json::from_slice(&body)
        .map_err(|e| TrustError::MalformedRevocations(e.to_string()))?;

    let root = roots
        .iter()
        .find(|k| k.id == list.root_key_id)
        .ok_or_else(|| TrustError::UnknownRootKey {
            wanted: list.root_key_id.clone(),
            known: roots.iter().map(|k| k.id.to_owned()).collect(),
        })?;

    let key = VerifyingKey::from_bytes(&root.public_key)
        .map_err(|e| TrustError::BadKeyMaterial(e.to_string()))?;
    verify_detached(&key, &body, &signature).map_err(TrustError::MalformedRevocations)?;

    Ok(list)
}

/// Splits `base64(body).base64(signature)` into its two decoded halves.
///
/// Shared by certificates and revocation lists, and returning a bare reason rather than a
/// [`TrustError`] so that each caller can say which document it was reading. A revocation
/// list reported as a malformed certificate would send somebody to look at the signing
/// setup instead of at the list.
fn split_signed(document: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (body, signature) = document
        .split_once('.')
        .ok_or_else(|| "no '.' separating body from signature".to_owned())?;

    let engine = base64::engine::general_purpose::STANDARD;
    let body = engine
        .decode(body)
        .map_err(|e| format!("body is not base64: {e}"))?;
    let signature = engine
        .decode(signature)
        .map_err(|e| format!("signature is not base64: {e}"))?;

    Ok((body, signature))
}

/// [`split_signed`], reporting as a certificate.
fn split_certificate(certificate: &str) -> Result<(Vec<u8>, Vec<u8>), TrustError> {
    split_signed(certificate).map_err(TrustError::MalformedCertificate)
}

/// Decodes the base64 detached signature published beside a manifest.
///
/// Public because the thing that fetches a signature should not have to grow a base64
/// dependency, and more importantly should not get to decide what a signature is. Sixty-
/// four bytes, and a file that is not that is a malformed artefact rather than a wrong one.
///
/// # Errors
/// [`TrustError::BadKeyMaterial`], which names re-running `plexos-sign` as the remedy.
pub fn decode_signature(encoded: &str) -> Result<[u8; 64], TrustError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| TrustError::BadKeyMaterial(format!("the signature is not base64: {e}")))?;

    bytes
        .try_into()
        .map_err(|_| TrustError::BadKeyMaterial("an Ed25519 signature is 64 bytes".to_owned()))
}

/// Decodes a base64 Ed25519 public key.
fn decode_key(encoded: &str) -> Result<VerifyingKey, TrustError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| TrustError::BadKeyMaterial(format!("public key is not base64: {e}")))?;

    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| TrustError::BadKeyMaterial("an Ed25519 public key is 32 bytes".to_owned()))?;

    VerifyingKey::from_bytes(&bytes).map_err(|e| TrustError::BadKeyMaterial(e.to_string()))
}

/// Checks one detached Ed25519 signature.
///
/// `verify_strict` rather than `verify`: it rejects small-order public keys and
/// non-canonical signature encodings, which is the difference between "this signature is
/// valid" and "this signature is valid and means what you think it means".
fn verify_detached(key: &VerifyingKey, message: &[u8], signature: &[u8]) -> Result<(), String> {
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| "an Ed25519 signature is 64 bytes".to_owned())?;

    key.verify_strict(message, &Signature::from_bytes(&signature))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    const ENGINE: base64::engine::general_purpose::GeneralPurpose =
        base64::engine::general_purpose::STANDARD;

    /// A keypair from a fixed seed. Deterministic on purpose: a failing signature test
    /// that cannot be reproduced is worse than no test.
    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// A manifest whose bytes are the thing signed, with `signing` filled in.
    ///
    /// Built as text rather than serialised from a struct, because that is how a real one
    /// arrives and because re-serialising is the exact mistake this module exists to
    /// avoid. The body is the frozen v1 fixture with its `signing` block replaced.
    fn manifest_bytes(key_id: &str, certificate: &str) -> Vec<u8> {
        let fixture = include_str!("../../plexos-types/tests/fixtures/manifest-v1.json");
        let head = fixture
            .split_once("\"signing\"")
            .expect("the fixture has a signing block")
            .0;
        format!(
            "{head}\"signing\": {{\n    \"key_id\": \"{key_id}\",\n    \
             \"certificate\": \"{certificate}\"\n  }}\n}}\n"
        )
        .into_bytes()
    }

    /// A certificate for `signing`, vouched for by `root`.
    fn certificate(root: &SigningKey, root_id: &str, signing: &SigningKey, key_id: &str) -> String {
        let body = format!(
            r#"{{"key_id":"{key_id}","public_key":"{}","not_after":"2027-01-01T00:00:00Z","root_key_id":"{root_id}"}}"#,
            ENGINE.encode(signing.verifying_key().to_bytes())
        );
        let signature = root.sign(body.as_bytes());
        format!(
            "{}.{}",
            ENGINE.encode(body.as_bytes()),
            ENGINE.encode(signature.to_bytes())
        )
    }

    fn roots(root: &SigningKey, id: &'static str) -> Vec<RootKey> {
        vec![RootKey {
            id,
            public_key: root.verifying_key().to_bytes(),
            development: true,
        }]
    }

    /// A policy trusting `roots`, revoking nothing.
    fn policy<'a>(roots: &'a [RootKey], now: Option<&'a str>) -> Policy<'a> {
        Policy {
            roots,
            revoked: &[],
            now,
        }
    }

    #[test]
    fn a_properly_signed_manifest_verifies_end_to_end() {
        // The accept path. With the root keys fixed by a constant this could not be
        // written at all, and
        // a suite that only ever exercised the refusal would have passed against a
        // verifier that accepted everything.
        let root = key(1);
        let signing = key(2);
        let cert = certificate(&root, "plexos-root-dev", &signing, "plexos-signing-test");
        let bytes = manifest_bytes("plexos-signing-test", &cert);
        let signature = signing.sign(&bytes);

        let raw = RawManifest::new(bytes);
        let verified = verify(
            &policy(
                &roots(&root, "plexos-root-dev"),
                Some("2026-07-29T00:00:00Z"),
            ),
            &raw,
            &signature.to_bytes(),
        )
        .expect("a correctly signed manifest");

        assert_eq!(verified.key_id, "plexos-signing-test");
        assert_eq!(verified.root_key_id, "plexos-root-dev");
        assert!(verified.development, "a test key is never a real root");
        assert_eq!(verified.manifest.sequence, 202_607_281_844);
    }

    #[test]
    fn one_flipped_byte_in_the_manifest_invalidates_it() {
        // The property that makes any of this worth doing. Note what is *not* changed:
        // the certificate is untouched and still valid, so this is specifically the
        // manifest-signature check catching a payload substitution.
        let root = key(1);
        let signing = key(2);
        let cert = certificate(&root, "plexos-root-dev", &signing, "plexos-signing-test");
        let bytes = manifest_bytes("plexos-signing-test", &cert);
        let signature = signing.sign(&bytes);

        // Inside the /usr image digest, so the document still parses and the failure is
        // unambiguously the signature rather than the reader.
        let mut tampered = bytes.clone();
        let at = tampered
            .windows(7)
            .position(|w| w == b"e3b0c44")
            .expect("the fixture's usr digest");
        tampered[at] = b'f';

        let raw = RawManifest::new(tampered);
        assert!(matches!(
            verify(
                &policy(&roots(&root, "plexos-root-dev"), None),
                &raw,
                &signature.to_bytes()
            ),
            Err(TrustError::ManifestNotSigned(_))
        ));
    }

    #[test]
    fn a_certificate_from_the_wrong_root_is_refused() {
        // The whole point of the two tiers: an attacker with a signing key of their own
        // still needs a root key to certify it, and root keys change only with the image.
        let real_root = key(1);
        let attacker_root = key(9);
        let signing = key(2);

        let cert = certificate(
            &attacker_root,
            "plexos-root-dev",
            &signing,
            "plexos-signing-test",
        );
        let bytes = manifest_bytes("plexos-signing-test", &cert);
        let signature = signing.sign(&bytes);

        let raw = RawManifest::new(bytes);
        assert!(matches!(
            verify(
                &policy(&roots(&real_root, "plexos-root-dev"), None),
                &raw,
                &signature.to_bytes()
            ),
            Err(TrustError::CertificateNotSigned(_))
        ));
    }

    #[test]
    fn a_manifest_naming_a_key_its_certificate_does_not_authorise_is_refused() {
        // Two halves from different places. Caught before the signature check, because
        // the signature would verify -- the manifest really was signed by the certified
        // key -- while `key_id` claims something else, and a reader believing the claim
        // would attribute the update to the wrong key during an incident.
        let root = key(1);
        let signing = key(2);
        let cert = certificate(&root, "plexos-root-dev", &signing, "the-real-key");
        let bytes = manifest_bytes("a-different-key", &cert);
        let signature = signing.sign(&bytes);

        let raw = RawManifest::new(bytes);
        match verify(
            &policy(&roots(&root, "plexos-root-dev"), None),
            &raw,
            &signature.to_bytes(),
        ) {
            Err(TrustError::KeyIdMismatch {
                manifest,
                certificate,
            }) => {
                assert_eq!(manifest, "a-different-key");
                assert_eq!(certificate, "the-real-key");
            }
            other => panic!("expected a key id mismatch, got {other:?}"),
        }
    }

    #[test]
    fn an_expired_certificate_is_refused_when_the_clock_can_be_believed() {
        let root = key(1);
        let signing = key(2);
        let cert = certificate(&root, "plexos-root-dev", &signing, "plexos-signing-test");
        let bytes = manifest_bytes("plexos-signing-test", &cert);
        let signature = signing.sign(&bytes);
        let raw = RawManifest::new(bytes);

        assert!(matches!(
            verify(
                &policy(
                    &roots(&root, "plexos-root-dev"),
                    Some("2028-01-01T00:00:00Z")
                ),
                &raw,
                &signature.to_bytes()
            ),
            Err(TrustError::CertificateExpired { .. })
        ));

        // And `None` means "this device cannot tell the time", which must not be treated
        // as "the certificate is fine" by accident -- it is, but only because the caller
        // decided that, which is why the decision is a parameter.
        assert!(
            verify(
                &policy(&roots(&root, "plexos-root-dev"), None),
                &raw,
                &signature.to_bytes()
            )
            .is_ok()
        );
    }

    #[test]
    fn a_build_with_no_root_keys_refuses_rather_than_accepts() {
        // The state this file shipped in for months, and the one that must fail closed. An
        // empty trust store is not "nothing to check against", it is "nothing can be
        // checked" -- and the tempting reading of an empty list is the other one.
        let raw = RawManifest::new(b"{}".to_vec());
        let nothing = Policy {
            roots: &[],
            revoked: &[],
            now: None,
        };
        assert_eq!(
            verify(&nothing, &raw, &[0; 64]).unwrap_err(),
            TrustError::NoRootKeys
        );
        assert_eq!(
            verify_revocations(&[], "anything").unwrap_err(),
            TrustError::NoRootKeys
        );
    }

    #[test]
    fn this_build_trusts_exactly_one_root_and_says_it_is_a_development_key() {
        // The constant is what an image actually ships with, and every test above runs
        // against keys made up locally -- so without this, ROOT_KEYS could go back to
        // empty and the whole suite would still pass while no appliance could update.
        assert_eq!(ROOT_KEYS.len(), 1, "one root key is compiled in");
        assert!(
            ROOT_KEYS.iter().all(|k| k.development),
            "a key whose private half is on a build host must say so, or the console \
             reports 'signed' about something weaker than the word implies"
        );
        assert!(ROOT_KEYS.iter().all(|k| !k.id.is_empty()));
        assert!(
            ROOT_KEYS.iter().all(|k| k.public_key != [0u8; 32]),
            "an all-zero key is a small-order point, not a key"
        );

        // And the policy an image uses is the one built from that constant, rather than
        // something a caller assembles and can assemble wrongly.
        assert_eq!(Policy::of_this_build(None).roots.len(), ROOT_KEYS.len());
        assert!(Policy::of_this_build(None).revoked.is_empty());
    }

    #[test]
    fn every_refusal_names_a_remedy() {
        // The rule plexos-gpu enforces with a test, applied here because this is where a
        // refusal is most likely to be read by somebody who cannot see the machine. A
        // message that says "signature invalid" and stops has reproduced the problem.
        let cases = [
            TrustError::NoRootKeys,
            TrustError::Unparsable("x".to_owned()),
            TrustError::MalformedCertificate("x".to_owned()),
            TrustError::UnknownRootKey {
                wanted: "a".to_owned(),
                known: vec![],
            },
            TrustError::CertificateNotSigned("x".to_owned()),
            TrustError::CertificateExpired {
                not_after: "2026-01-01T00:00:00Z".to_owned(),
                now: "2027-01-01T00:00:00Z".to_owned(),
            },
            TrustError::KeyIdMismatch {
                manifest: "a".to_owned(),
                certificate: "b".to_owned(),
            },
            TrustError::KeyRevoked {
                key_id: "a".to_owned(),
            },
            TrustError::ManifestNotSigned("x".to_owned()),
            TrustError::BadKeyMaterial("x".to_owned()),
            TrustError::MalformedRevocations("x".to_owned()),
        ];

        for case in cases {
            let message = case.to_string();
            assert!(
                message.contains("Remedy:"),
                "{case:?} has no remedy: {message}"
            );
        }
    }

    /// A root-signed revocation list, in the encoding a device reads.
    fn revocations(root: &SigningKey, root_id: &str, counter: u64, revoked: &[&str]) -> String {
        let list: Vec<String> = revoked.iter().map(|s| format!("\"{s}\"")).collect();
        let body = format!(
            r#"{{"counter":{counter},"root_key_id":"{root_id}","revoked":[{}]}}"#,
            list.join(",")
        );
        format!(
            "{}.{}",
            ENGINE.encode(body.as_bytes()),
            ENGINE.encode(root.sign(body.as_bytes()).to_bytes())
        )
    }

    #[test]
    fn a_manifest_signed_by_a_revoked_key_is_refused_although_the_signature_is_good() {
        // The case expiry cannot cover. The attacker's certificate is valid, their
        // signature verifies, and every other check in this module says yes -- which is
        // precisely the situation a revocation list exists for.
        let root = key(1);
        let signing = key(2);
        let cert = certificate(&root, "plexos-root-dev", &signing, "leaked-key");
        let bytes = manifest_bytes("leaked-key", &cert);
        let signature = signing.sign(&bytes);
        let raw = RawManifest::new(bytes);
        let roots = roots(&root, "plexos-root-dev");

        // Without the list, this is a perfectly good update.
        assert!(verify(&policy(&roots, None), &raw, &signature.to_bytes()).is_ok());

        let revoked = vec!["leaked-key".to_owned()];
        match verify(
            &policy(&roots, None).revoking(&revoked),
            &raw,
            &signature.to_bytes(),
        ) {
            Err(TrustError::KeyRevoked { key_id }) => assert_eq!(key_id, "leaked-key"),
            other => panic!("expected a revoked key, got {other:?}"),
        }
    }

    #[test]
    fn a_revocation_list_is_believed_only_when_a_root_key_signed_it() {
        // Otherwise anyone on the wire could revoke the publisher's key and stop every
        // appliance updating -- a denial of service with no forgery required.
        let root = key(1);
        let attacker = key(9);
        let roots = roots(&root, "plexos-root-dev");

        let list = verify_revocations(
            &roots,
            &revocations(&root, "plexos-root-dev", 3, &["leaked-key"]),
        )
        .expect("a root-signed list");
        assert_eq!(list.counter, 3);
        assert_eq!(list.revoked, vec!["leaked-key".to_owned()]);

        assert!(matches!(
            verify_revocations(
                &roots,
                &revocations(&attacker, "plexos-root-dev", 4, &["the-real-key"])
            ),
            Err(TrustError::MalformedRevocations(_))
        ));
        assert!(matches!(
            verify_revocations(&roots, "not even two fields"),
            Err(TrustError::MalformedRevocations(_))
        ));
    }

    #[test]
    fn an_older_revocation_list_cannot_un_revoke_anything() {
        // The list from before a revocation is genuinely root-signed, so replaying it is
        // the obvious attack once revocation exists at all.
        let held = Revocations {
            counter: 3,
            root_key_id: "plexos-root-dev".to_owned(),
            revoked: vec!["leaked-key".to_owned()],
        };
        let older = Revocations {
            counter: 2,
            ..held.clone()
        };
        let same_number_fewer_entries = Revocations {
            counter: 3,
            revoked: Vec::new(),
            ..held.clone()
        };
        let newer = Revocations {
            counter: 4,
            ..held.clone()
        };

        assert!(!older.supersedes(&held));
        assert!(!same_number_fewer_entries.supersedes(&held));
        assert!(newer.supersedes(&held));
        assert!(held.supersedes(&Revocations::none()));
        assert!(Revocations::none().revoked.is_empty());
    }

    #[test]
    fn a_certificate_must_be_two_base64_fields() {
        assert!(matches!(
            split_certificate("no-dot-here"),
            Err(TrustError::MalformedCertificate(_))
        ));
        assert!(matches!(
            split_certificate("!!!.aaaa"),
            Err(TrustError::MalformedCertificate(_))
        ));

        let engine = base64::engine::general_purpose::STANDARD;
        let certificate = format!("{}.{}", engine.encode(b"body"), engine.encode(b"sig"));
        let (body, signature) = split_certificate(&certificate).expect("well formed");
        assert_eq!(body, b"body");
        assert_eq!(signature, b"sig");
    }

    #[test]
    fn a_key_must_be_thirty_two_bytes() {
        let engine = base64::engine::general_purpose::STANDARD;
        assert!(matches!(
            decode_key(&engine.encode([0u8; 31])),
            Err(TrustError::BadKeyMaterial(_))
        ));
        assert!(matches!(
            decode_key("not base64!"),
            Err(TrustError::BadKeyMaterial(_))
        ));
    }

    #[test]
    fn an_all_zero_key_is_rejected_rather_than_accepted() {
        // A small-order public key. `verify_strict` is what refuses it, and this pins
        // that the strict variant is the one being called: with plain `verify`, some
        // signatures validate under such a key regardless of the message, which turns
        // "signed" into a statement about nothing.
        let engine = base64::engine::general_purpose::STANDARD;
        let key = decode_key(&engine.encode([0u8; 32]));
        match key {
            // Rejected at decode; equally good.
            Err(TrustError::BadKeyMaterial(_)) => {}
            Ok(key) => assert!(
                verify_detached(&key, b"anything", &[0u8; 64]).is_err(),
                "a small-order key must not verify anything"
            ),
            Err(other) => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn a_clock_that_predates_the_image_is_not_used_to_judge_expiry() {
        // This appliance has no time synchronisation. A clock believed while wrong
        // refuses every future update, which is indistinguishable from a broken update
        // path -- and unlike an expired certificate, nobody would know where to look.
        assert!(!expiry_is_checkable(
            "2020-01-01T00:00:00Z",
            "2026-07-29T00:00:00Z"
        ));
        assert!(expiry_is_checkable(
            "2026-08-01T00:00:00Z",
            "2026-07-29T00:00:00Z"
        ));
    }
}
