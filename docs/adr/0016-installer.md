# ADR-0016: The installer, and the first boot after it

**Status:** Proposed
**Date:** 2026-07-30

## Context

Every PlexOS installation so far has been `dd` onto a disk by the person who built the
image. That is why nothing else exists: the only user was the author, and the author has a
build host. A machine handed to anybody else needs an installer, and the appliance has
reached the point where everything else it does is finished enough to make that the largest
remaining gap.

Four facts about the machine shape what follows, and all four were read off it rather than
assumed:

- **The reference laptop has Windows on its internal disk.** `nvme0n1p1` is labelled
  `SYSTEM`, a 465 GiB Kingston. The USB stick PlexOS boots from is `sda`, `removable=1`.
  An installer that picks the wrong one destroys somebody's computer, and this one has to
  be run on exactly that machine.
- **The image can partition nothing.** No `sfdisk`, no `sgdisk`, no `parted`, no
  `wipefs`, no `mkfs.vfat`. It does have `mkfs.xfs`, `partprobe` and `blkid`.
- **The layout already has one definition.** `plexos_types::partition` owns every GUID,
  size and label (ADR-0003), and `plexos-layout` already emits it as an `sfdisk` script,
  with tests. Nothing on the device has ever consumed that output.
- **The console is the only interface this appliance has**, and since ADR-0014's revision
  it is served over TLS. There is no keyboard anybody is expected to use and the attached
  screen exists to print a URL and a fingerprint.

## Decision

### The installer is a mode of the same image, and it installs itself

A machine boots PlexOS from a USB stick, its console prints a URL, and from a browser you
choose a disk and press a button. What gets written to that disk is **the system that is
currently running** — its `/usr` partition, its verity hash tree, and the UKI from the ESP
it booted.

Nothing is downloaded and there is no second artefact to build, sign or keep in step. It
also means the thing installed is the thing that has already been verified by dm-verity and
observed to boot on this hardware, which is a stronger guarantee than any installer payload
could carry.

### The partition table is written by this code, and verified by somebody else's

`plexos_types::gpt` produces the bytes: protective MBR, primary and backup headers, and the
entry array, straight from the constants ADR-0003 froze. No new package enters the image.

The alternative was `sfdisk`, a sub-option of a package the image already carries, fed by
the `sfdisk` script `plexos-layout` has emitted and tested since before anything could
consume it. It was the first choice and it was wrong, for a reason this repository has
written down three times: **a program in the image is not a program that can do the job.**
`erofs-utils` without `lz4`, `busybox tar` without `xz`, `busybox losetup` without
`--show` — each present, each unable, each failing minutes into a long operation. Adding a
fourth such dependency to the most destructive operation in the system is a bet against our
own history.

What makes writing it ourselves acceptable is not confidence in the code. It is that the
result is **checked by tools that are not this code**: the tests write a table and run
`sgdisk --verify` and `sfdisk --list` over it on the build host, where both already exist.
That is the same rule the verity digest follows by being pinned against `sha256sum`, and
the same one that caught a GUID pair which was well-formed, unique, correctly paired, and
not the pair the specification defines.

`BR2_PACKAGE_DOSFSTOOLS` is still added for the target, because the ESP is FAT and the
image cannot currently make one. That is a filesystem, not a partition table, and writing
one of those ourselves would be a different and much worse trade.

### The disk is chosen against evidence, not from a list

The console shows every disk with its model, size, whether it is removable, and **what is
already on it** from `blkid` — so `KINGSTON SA2000M8500G, 465 GiB, contains SYSTEM (vfat),
Basic data partition (ntfs)` reads as somebody's Windows rather than as `/dev/nvme0n1`.

Two refusals are structural rather than advisory:

- **The disk the installer is running from is never offered.** It is found by resolving the
  partitions currently mounted, not by trusting `removable`.
- **A disk with anything on it requires its name to be typed.** Not a checkbox: a
  confirmation you can click through is one people do click through, and this is the one
  operation in the whole system that destroys data that was not ours.

### `/var` is created empty and the first boot does the rest

The installer makes the filesystems and copies three things; it does not populate state.
`plexos-init` already creates the `/var` layout on a boot that finds none (ADR-0009), and
having one path do it means the installed machine and a `dd`ed one are in the same state on
first boot rather than in two states that drift.

**Reinstalling over an existing PlexOS deliberately does not preserve `/var` in this
version.** Keeping a media database across a reinstall is a real thing to want and a real
migration problem; pretending to do it and getting it subtly wrong is worse than saying it
is not offered.

### The first boot is a flow, not a page of settings

Nearly every step already exists: claiming the device (ADR-0013), the network, hostname and
timezone (ADR-0008), installing Plex (ADR-0010), mounting shares. What does not exist is any
sense of *order*, or of a machine that is not set up yet — a fresh appliance shows the same
console as one that has been running for a year, with the Plex card offering an install and
nothing saying that is the next thing to do.

The wizard is that ordering, over the endpoints that are already there. It is finished when
Plex is answering, and after that the console is what it is today.

## Alternatives considered

**A separate installer image.** The conventional answer, and it doubles the build, the
signing and the number of artefacts that can be out of step. It also installs something
other than what is running, which throws away the guarantee above.

**Installing beside an existing operating system.** Shrinking an NTFS partition to fit
PlexOS next to Windows. Rejected for this version and probably for every version: it is the
single most dangerous thing an installer can do, this appliance is meant to own its machine,
and the honest alternative — say the disk will be erased, and refuse if that is not what
somebody wants — costs nothing to be sure about.

**A text installer on the attached screen.** Rejected for the reason the console exists:
the reference laptop's panel is 2160x1440 and reading a diagnostic off it is the thing this
project has spent months arranging not to need. The screen prints a URL and a certificate
fingerprint; everything else is a browser.

## Consequences

- The image grows `mkfs.vfat` and a GPT writer, and the installer is code that runs as root and
  writes to a whole disk. It is the most dangerous thing in the repository and its tests
  have to be about refusals rather than about success.
- **The layout stops being a build-time artefact.** `plexos-layout` output has until now
  been consumed only by `post-image.sh` on a build host; it becomes something a device
  executes, which makes ADR-0003's frozen numbers load-bearing in a second place.
- Installing what is running means an installed machine's first update comes from the same
  place its image did, so the anti-rollback floor (ADR-0006) starts at the installer's
  sequence rather than at zero. That is correct and worth knowing.
- Nothing here has been tested on hardware, and the hardware in question has somebody's
  Windows installation on it. The first end-to-end run needs a disk nobody minds losing.
