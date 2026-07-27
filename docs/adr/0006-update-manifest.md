# ADR-0006: Update manifest format, signing, and key rotation

**Status:** Accepted
**Date:** 2026-07-27

## Context

The update manifest is the only thing a deployed device parses before it can be told
anything new. Every constraint follows from that:

- A device running v0.1 will one day be offered a manifest produced by v3 tooling. It
  must fail comprehensibly rather than crash or, far worse, misinterpret it.
- The signing key will need to be rotated. If rotation is not designed in on day one,
  the key can never change without abandoning every deployed device.
- A signed manifest is valid forever unless something prevents replay. An attacker who
  can serve a device an old, legitimately signed manifest can downgrade it into a
  version with a known vulnerability.

## Decision

### Format

JSON, with `manifest_version` as an integer that must be the first field parsed. A
device refuses any `manifest_version` it does not implement, and reports the version
it saw. Within a supported version, unknown fields are ignored so that additive
changes do not require a version bump.

### Signing

**Ed25519 over the raw manifest bytes, as a detached signature.** The manifest is
signed as an opaque byte string — never re-serialised, never canonicalised. JSON
canonicalisation is a well-known source of signature-bypass bugs, and the only
reliable way to avoid them is to not depend on canonicalisation at all.

### Key rotation

Two tiers, deliberately:

- **Root keys** are baked into the `/usr` image and therefore covered by the verity
  root hash and the UKI signature (ADR-0004). They are held offline, are used only to
  sign signing-key certificates, and change only via an OS update.
- **Signing keys** sign manifests. Each has a certificate, signed by a root key, with
  an expiry. The certificate travels with the manifest, so a device needs no prior
  knowledge of the current signing key.

Compromise of a signing key is handled by a **revocation list**, itself root-signed
and carrying a monotonic counter so it cannot be rolled back to a pre-revocation state.

### Anti-rollback

Every manifest carries a monotonic `sequence` integer, distinct from the
human-readable version. A device persists the highest sequence it has accepted and
refuses anything lower. Version strings are for humans; `sequence` is the security
boundary.

### Delta transport designed in, not implemented

Each payload declares a list of `sources`. v0.1 emits and understands only
`kind: "full"`. The schema already admits `kind: "chunked"` with an index and a chunk
store, so delta updates can be introduced later by *servers* without a manifest
version bump — old devices ignore the chunked source and take the full one.

This is the concrete reason this ADR exists before any updater code: the ability to
add delta transport in 2027 depends entirely on a field being present in the schema in
2026.

## Alternatives considered

**Full TUF.** The correct answer for a large update ecosystem, with role separation,
threshold signing, and expiry across the board. Rejected as disproportionate for a
single-publisher appliance; the design above deliberately borrows its load-bearing
ideas — offline root keys, key rotation, monotonic anti-rollback — without the full
role model. If PlexOS ever gains third-party publishers, this ADR is superseded by
adopting TUF properly.

**Signing the payload only, with an unsigned manifest.** Rejected: the manifest is
where anti-rollback and key-rotation state lives, and unsigned metadata means an
attacker chooses which signed payload a device installs.

**CBOR or Protobuf instead of JSON.** More compact and less ambiguous to parse. The
gain is negligible for a document of a few kilobytes, and being able to read a
manifest during a support call has real value.

**GPG signatures.** Rejected: a large attack surface and an awkward library story in
Rust, for no benefit over Ed25519 here.

## Consequences

- Root key custody becomes an operational responsibility from the first public image.
  Losing every root key means no device can ever be updated again.
- The signature covers exact bytes, so any tool that reformats a manifest breaks it.
  Manifests must be treated as immutable artifacts once signed.
- Devices must persist accepted-sequence and revocation state across updates, in `/var`
  (ADR-0009), and that state must survive rollback.
- `manifest_version` bumps are expensive and should be rare. Additive optional fields
  are the intended way to evolve the format.
- The schema is defined in `plexos-types::manifest`, with fixture-based tests that
  fail if the wire format changes. Those fixtures are the actual contract.
