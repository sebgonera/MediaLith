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

## What implementing it changed, 2026-07-30

The schema above was written before anything had been built, and three of its assumptions
did not survive contact with the artefacts. All three were corrected in the schema rather
than worked around in the updater, because no appliance had ever parsed a manifest — the
deployed ones read an improvised `update.json` — so it was the last moment at which this
was an edit rather than a migration.

**There is one UKI per slot, not one per release.** `plexos.slot=` is on the kernel
command line *inside* the UKI, and the appliance cannot build one: that needs `objcopy`,
which is not in the image and should not be. `uki` is now `{ "a": …, "b": … }`. A device
that wrote slot B and installed slot A's entry would boot the slot it was already running
— an update that installs, reboots, and changes nothing.

**`os_version` cannot express the version an image carries.** It is `MAJOR.MINOR.PATCH`,
and PlexOS publishes `0.1.0.202607281844`. That full string is what `os-release` carries,
what names the boot entry, and what `systemd-boot` orders entries by. A new `release` field
carries it verbatim rather than composing it from parts, since recomposition is how a
publisher and a device come to disagree about the version of the thing they are both
holding. The parser refuses a manifest whose two version fields have drifted.

**Sources are file names, not URLs.** An absolute URL fixed at signing time ties a bundle
to the address it was built for, so moving it — which is every publish this project has
done — would mean re-signing with a key that is supposed to be offline. A source is
resolved against wherever the manifest itself was fetched from, and a name that is not a
plain file name is refused rather than sanitised.

Two further decisions follow from the same implementation:

**`sequence` is the build stamp.** `202607281844` out of the version string. It is
monotonic by construction, needs no counter to be kept anywhere, and cannot disagree with
the release it describes. It also means the anti-rollback floor has a second source: the
running image's own stamp, so a machine that has never taken an update — which is every
machine installed by `dd` — still cannot be talked below what it is executing.

**Certificate expiry is checked only when the clock can be believed.** This appliance has
an RTC and no time synchronisation. A wrong clock that is believed refuses every future
update, which from outside is indistinguishable from a bricked update path. A clock reading
earlier than the image's own build stamp is definitely wrong, because an image cannot
predate itself; in that case expiry goes unchecked, which costs the narrow protection of
expiry and keeps the machine updatable. Revocation covers the case expiry was for.

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
