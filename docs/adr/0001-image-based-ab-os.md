# ADR-0001: Immutable, image-based OS with A/B slots

**Status:** Accepted
**Date:** 2026-07-27

## Context

PlexOS runs unattended on a machine in a cupboard. Nobody watches it update, and
nobody is present to fix it when an update goes wrong. It must survive power loss
mid-update, a corrupted download, and a kernel that no longer boots on the hardware.

The classic distribution model — a package manager applying hundreds of small
transactions to a mutable root filesystem — cannot offer this. A package transaction
interrupted by power loss leaves a system in a state that is neither the old one nor
the new one, and configuration drift accumulates until two machines running "the same
version" behave differently.

## Decision

PlexOS is immutable and image-based:

- `/usr` is a read-only filesystem image, verified by dm-verity, in one of two slots.
- `/` is a tmpfs assembled at boot. It does not persist.
- `/etc` is an overlay: factory defaults from the image below, persistent changes above.
- `/var` is the single writable, persistent partition.
- An update writes a complete new image to the inactive slot and reboots into it.
  Nothing is modified in place, so there is no partially-applied state.
- A slot that fails to boot or fails its health check is abandoned automatically.

There is no package manager on the device.

## Alternatives considered

**Mutable root with a package manager (Debian/Fedora model).** Familiar, and the
whole ecosystem assumes it. Rejected: no atomicity, no rollback, and configuration
drift is unavoidable. Every one of these is a support burden we would carry forever.

**OSTree / bootc.** Genuinely good, atomic, with rollback, and battle-tested in
Fedora Silverblue. Rejected for this project because it brings a content-addressed
object store and a substantial C dependency to manage, and because whole-image A/B
composes more directly with dm-verity: the root hash goes on the kernel command line
and the trust chain closes with no extra moving parts.

**Container-only OS (the host is trivial, everything runs in containers).** Rejected
as insufficient on its own — it answers "how do apps update", not "how does the
kernel update". PlexOS does use an image boundary for Plex itself (ADR-0007), but the
host needs its own atomic update story regardless.

## Consequences

- Two `/usr` slots means the space for `/usr` is doubled. At roughly 1 GiB per slot
  this is irrelevant on any plausible target disk.
- Users cannot install extra packages. This is intended: the appliance is the product,
  and an unconstrained package set would destroy the CVE-maintenance story.
- Every change, including a one-line fix, requires a full image build and a reboot.
  Build and test tooling must therefore be fast enough to make this painless — this is
  a direct input into ADR-0002.
- Anything that must persist has to be explicitly placed in `/var`. Any code writing
  outside `/var` and expecting it to survive is a bug, and it will be caught on the
  first reboot rather than silently working for months.
