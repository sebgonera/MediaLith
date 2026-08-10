# ADR-0004: Verified boot via dm-verity and signed UKIs

**Status:** Accepted
**Date:** 2026-07-27

## Context

An appliance that updates itself unattended over the network needs to be certain that
what it booted is what we built. Verifying a downloaded image once, at install time,
is not enough: it says nothing about the disk that has been sitting in a cupboard for
two years, or about an attacker with physical access.

The verification also needs to be continuous rather than one-shot. Hashing a 1 GiB
image at every boot costs seconds — unacceptable against a 5-second boot target — and
protects nothing after the check completes.

## Decision

A single chain, closed at every link:

1. Firmware verifies the bootloader against Secure Boot keys.
2. The bootloader launches a **Unified Kernel Image**: kernel, `plexos-init`, and the
   kernel command line in one PE binary, signed as a unit.
3. The command line inside that UKI carries the **dm-verity root hash** of the `/usr`
   image the UKI expects.
4. `plexos-init` sets up dm-verity over the `/usr` partition using that root hash.
   Every block is verified against the Merkle tree **on read**, forever, at negligible
   cost.

The critical property: the root hash is inside the signed UKI. The signature on the
UKI therefore transitively covers every byte of `/usr`. There is no second signature
to check, no separate image-verification step to get wrong, and no window in which an
unverified `/usr` is mounted.

A verity failure is fatal. `plexos-init` does not fall back to mounting unverified;
it fails the boot, which hands control to the rollback path in ADR-0005.

## Alternatives considered

**Hash the whole image at boot and compare.** Simple. Rejected: seconds of boot time,
and no protection at all once the image is mounted — an attacker with write access to
the block device can modify it after the check.

**IMA/EVM with per-file signatures.** Finer-grained and allows a mutable root.
Rejected as considerably more complex to operate, and unnecessary when `/usr` is a
single immutable image — dm-verity is the right tool for exactly this shape.

**A verity signature partition (DPS `usr-verity-sig`), with the kernel verifying the
root hash against a keyring rather than taking it from the command line.** This is
what systemd does, and it allows one UKI to boot several signed images. Rejected
because PlexOS ships kernel and `/usr` as one atomic unit anyway, so the flexibility
buys nothing and costs an extra key-management surface. The partition type GUID is
noted but unused; adopting it later is additive.

**No Secure Boot.** Rejected as a design position, though it remains the default
*operational* state on machines where the user has not enrolled our key. dm-verity
still protects against offline modification of `/usr` in that case; what is lost is
protection of the UKI itself.

## Consequences

- Kernel and `/usr` are versioned and updated together, always. They cannot be
  independently rolled back.
- Building an image requires computing the verity tree first, then embedding the
  resulting root hash into the UKI, then signing. The build has a strict ordering
  dependency that image tooling must enforce.
- No initramfs: the kernel needs storage, erofs, and dm-verity built in.
- Secure Boot key handling is decided by [ADR-0017](0017-secure-boot-keys.md): PlexOS signs
  with its own `db` key and enrolment is a physical act at the machine. The paragraph
  below is what that ADR was written to answer, kept because it states the choice.
- Secure Boot key handling is **not decided by this ADR**. Shipping a distribution
  under Secure Boot means either asking users to enrol a PlexOS key in firmware or
  going through Microsoft's shim signing process. This must be resolved before the
  first public image; it does not block development, because the chain above works
  identically with self-signed keys in a test environment.
- Debug builds need a documented way to boot an unsigned, unverified image. This must
  be a separate build variant, never a runtime flag on a production image.
