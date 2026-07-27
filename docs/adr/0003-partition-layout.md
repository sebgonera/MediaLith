# ADR-0003: Partition layout and on-disk contract

**Status:** Accepted
**Date:** 2026-07-27

## Context

The partition layout is the single most expensive decision in the project to reverse.
Once an image has been installed on a device, changing partition sizes, ordering, or
type GUIDs means that device can no longer be updated — only reinstalled, losing its
Plex library in the process.

It therefore has to be specified before anything is built, with room for growth
designed in rather than discovered later.

## Decision

GPT on a UEFI x86-64 system. Six partitions, fixed order:

| # | Purpose | Size | Filesystem |
| --- | --- | --- | --- |
| 1 | ESP | 512 MiB | FAT32 |
| 2 | `usr_a` | 1 GiB | erofs, read-only |
| 3 | `usr_a` verity hashes | 32 MiB | dm-verity hash tree |
| 4 | `usr_b` | 1 GiB | erofs, read-only |
| 5 | `usr_b` verity hashes | 32 MiB | dm-verity hash tree |
| 6 | `var` | remainder | XFS |

Partition **type** GUIDs come from the Discoverable Partitions Specification, so that
standard tooling (`sfdisk`, `blkid`, `systemd-repart`) understands a PlexOS disk
without PlexOS-specific knowledge. Slot identity is carried in the partition **label**
(`usr_a` / `usr_b`), not the type GUID — both slots share a type GUID by design, since
they are interchangeable.

The canonical GUID values are defined once, in code, in
`plexos-types::partition`. They are pinned there with a test so that a typo cannot
reach an image silently.

### Sizing rationale

**ESP at 512 MiB** holds two Unified Kernel Images plus the bootloader, with headroom
for a third during an update and for a UKI that grows as drivers are built in. A
kernel that outgrows a cramped ESP mid-project is a classic way to be forced into a
repartition, so this is generously sized; the cost is negligible.

**`/usr` at 1 GiB** against a target image size of 150–250 MiB. This is a deliberate
4× margin. It is the number most likely to be regretted, and it cannot be raised in
the field.

**`/var` takes the remainder.** The Plex library, thumbnails, and metadata grow
without bound and are the only thing on the disk that users care about.

### Filesystem choices

**erofs** for `/usr`: designed for read-only images, compact, low-overhead random
access, and mainline. Squashfs would also work; erofs has better random-read behaviour,
which matters when every binary on the system is faulted in from it.

**XFS** for `/var`: mature, excellent with large files and parallel I/O, which is what
a media library is. btrfs was considered for snapshots, but snapshotting a Plex library
is not a use case that justifies the extra failure modes; snapshots of *configuration*
are handled at the application layer instead.

**Separate verity partitions** rather than a hash tree appended to the image. Slightly
more partitions, but each artifact is independently addressable and replaceable, which
keeps the updater simple and matches the Discoverable Partitions Specification layout.

## Alternatives considered

**Full root A/B rather than `/usr` A/B.** Simpler to explain. Rejected because it
forces a decision about what in `/` is mutable and what is not; a tmpfs root with a
read-only `/usr` makes that boundary structural instead of conventional.

**A dedicated partition per Plex app image version.** Rejected: the number of
concurrent versions is a policy choice that should not be frozen into the GPT.
App images are files under `/var` (ADR-0007).

**Dynamic partitioning via `systemd-repart` at first boot.** Attractive for growing
`/var` to fill an arbitrary disk, and a good candidate for a later revision. Rejected
for v1 because it makes the on-disk contract depend on runtime behaviour, and the
contract is the thing we are trying to make static.

## Consequences

- Total fixed overhead before `/var` is roughly 2.6 GiB. Irrelevant on any disk that
  would hold a media library.
- The layout supports exactly two slots. Three-slot schemes are foreclosed.
- Non-UEFI and non-x86-64 systems are not addressed. ARM64 will need its own type
  GUIDs from the same specification and its own ADR.
- Because slot identity lives in partition labels, a disk imaging tool that drops
  labels produces an unbootable system. The installer must be the only supported way
  to write a PlexOS disk.
