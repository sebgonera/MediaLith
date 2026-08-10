# ADR-0017: Secure Boot key custody, and who enrols them

**Status:** Accepted
**Date:** 2026-08-10

## Context

[ADR-0004](0004-verified-boot.md) specified the whole boot chain and closed every link but
the first. Its own consequences say so:

> Secure Boot key handling is **not decided by this ADR**. Shipping a distribution under
> Secure Boot means either asking users to enrol a PlexOS key in firmware or going through
> Microsoft's shim signing process. This must be resolved before the first public release.

Everything below that link works and has been demonstrated on hardware: the UKI carries the
dm-verity root hash of the `/usr` it expects, `plexos-init` sets verity up from it, and a
verity failure fails the boot into ADR-0005's rollback. What has never worked is the step
before all of it — firmware deciding whether to launch the bootloader at all — because
nothing said whose key it should trust, and so every image PlexOS has ever built required
Secure Boot to be turned off.

That is not a small caveat. With Secure Boot off, an attacker with brief physical access
replaces the bootloader or the UKI and the whole verity chain verifies exactly what they
put there. The chain is only as good as the first signature.

## Decision

**PlexOS signs with its own keys, and enrolment is a physical act at the machine.**

Three keys, matching UEFI's hierarchy, generated once by
`tools/make-secureboot-keys.sh` and kept in `~/.plexos-keys/secureboot` outside the
repository:

| Key | Signs |
| --- | --- |
| `PK` — Platform Key | changes to KEK; the root of the machine's hierarchy |
| `KEK` — Key Exchange Key | changes to the signature databases |
| `db` — Signature Database | **the bootloader and both UKIs**, at build time |

Only `db` signs artefacts. `PK` and `KEK` exist so that what is enrolled is a hierarchy
rather than a lone database entry: firmware returning to User Mode wants a PK, and a
machine whose PK belongs to nobody cannot have its databases updated later without being
cleared entirely.

`post-image.sh` signs the bootloader **and** both UKIs through one function, and verifies
each signature with `sbverify` immediately after making it. An unsigned build stays
possible and says so on every line it does not sign.

Enrolling `db.cer` into a machine's firmware is done by a person, in the firmware's own
setup screens. Nothing in PlexOS writes to the platform databases.

## Alternatives considered

**Microsoft's shim.** The route that works on any machine without touching firmware,
because shim is already trusted by everything shipped since 2012. Rejected *for now*, not
on the merits: it requires being an organisation, passing shim review, and waiting, and it
buys nothing until PlexOS is installed by people who did not build it. It remains the
answer the day that changes, and nothing here forecloses it — a shim-based chain still
needs the artefacts signed by our own key, which is exactly what this ADR sets up.

**Signing with a key enrolled through MOK.** A subset of the above: MokManager comes with
shim, so it needs shim, so it needs Microsoft.

**Leaving Secure Boot off and relying on dm-verity.** What PlexOS did until now, and what
it will still do on any machine whose firmware cannot enrol a custom key. Rejected as the
*design position* for the same reason ADR-0004 rejected it: verity proves that `/usr`
matches the root hash in the UKI, and proves nothing about who wrote the UKI.

**Shipping a key in the image for the appliance to enrol itself.** Rejected outright. A
system that can add a key to the platform database from userspace is a considerably more
interesting target than anything the key protects.

## Consequences

- **A person must enter firmware setup once per machine.** For an appliance whose
  installer already requires physical presence and a typed disk name (ADR-0016), that is
  in keeping. For a machine sent to somebody else, it is a real obstacle and the reason
  shim exists.
- **Firmware that offers no custom key enrolment cannot run PlexOS under Secure Boot.**
  Some consumer firmware does not. Those machines keep working exactly as today, with
  Secure Boot off, and the console already reports the command line that says so.
- **The `db` private key lives on the build host**, alongside the update signing key and
  with the same caveat: it is a development key, and every place that reports a signature
  says as much. Losing it means a machine that has enrolled it cannot be given a new
  signed image without going back into firmware setup.
- **Updates need no new work.** The bundle carries UKIs signed at build time, and the
  update path copies them to the ESP unchanged. The bootloader is written only when an
  image is installed, so it is signed by the same build.
- **A signed image on a machine that has not enrolled the key does not boot**, and the
  firmware's message will not mention PlexOS. That is the failure this needs documenting
  for, and `docs/DEVELOPMENT.md` documents it.

## What has been demonstrated

The build half, on the build host: `sbsign` over the bootloader and both UKIs, each
verified against `db.crt` after the fact, and the bootloader checked **as extracted from
the ESP of the built image** rather than before it was copied there.

**Nothing has yet been enrolled in any firmware, and no machine has booted this with
Secure Boot on.** Until that happens this ADR describes a chain whose last link is
untested, which is the same state ADR-0004 was in before the reference laptop booted it.
