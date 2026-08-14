# MediaLith Architecture

This document describes the system as designed. Individual decisions, with their
alternatives and consequences, live in [`docs/adr/`](adr/).

## 1. Shape of the system

MediaLith is an **image-based appliance OS**. It has no package manager on the device
and no in-place upgrade path. The unit of change is a whole `/usr` image, written to
whichever of two slots is not currently running, and activated by a reboot.

```
                    +-------------------------------------------+
   Secure Boot ---> | UKI (signed, one artifact):                |
                    |   kernel + initrd (plexos-init) + cmdline  |
                    |   cmdline carries the dm-verity root hash  |
                    +---------------------+---------------------+
                                          |
                                          v
                    +-------------------------------------------+
                    | dm-verity over /usr_{a,b}  (read-only)     |
                    +---------------------+---------------------+
                                          |
                                          v
       tmpfs /  <---- assembled by plexos-init ----> /etc overlay
                                          |
                                          v
                    +-------------------------------------------+
                    | /var (persistent, single partition)        |
                    |   Plex library, config, app images, logs   |
                    +-------------------------------------------+
```

The root filesystem is a tmpfs assembled at boot. Nothing outside `/var` survives a
reboot, which makes "reset to factory state" a one-line operation and makes
configuration drift structurally impossible.

Because the root hash of the `/usr` image is on the kernel command line, and the
command line is inside the Secure Boot–signed UKI, the signature on the UKI covers
the entire contents of `/usr`. There is no separate image-signature check to get
wrong. See [ADR-0004](adr/0004-verified-boot.md).

## 2. Boot sequence

1. Firmware verifies and launches `systemd-boot` (a standalone EFI application; it
   does not pull in systemd as PID 1).
2. `systemd-boot` selects a UKI from the ESP. Entry filenames carry a try counter, so
   a UKI that fails to boot three times is skipped in favour of the other slot. See
   [ADR-0005](adr/0005-bootloader-and-rollback.md).
3. The kernel starts `plexos-init` as PID 1 from the UKI's initrd section — no
   separate initramfs artifact exists (see §3).
4. `plexos-init` sets up dm-verity over the `/usr` partition for the slot named on the
   command line, assembles the root at `/sysroot` — verified `/usr`, persistent `/var`,
   `/etc` overlay — and `switch_root`s into it.
5. `plexos-init` runs state migrations if `/var` was written by an older layout
   version ([ADR-0009](adr/0009-persistent-state.md)).
6. Services start under `plexos-init`'s supervisor: `plexosd`, `plexos-gpu` self-test,
   Plex Media Server, then optional `plexos-shares` exports.
7. Once `plexosd` reports the system healthy, it clears the boot try counter, making
   the current slot permanent.

Target: power-on to Plex accepting connections in under 5 seconds, excluding firmware.

Step 7 is the load-bearing one. "Healthy" must mean more than "PID 1 is alive" —
otherwise a broken update that still boots will never roll back. The health gate
requires `plexosd` responding, `/var` mounted read-write, and Plex's HTTP port
accepting a request **on loopback**.

That last word is deliberate. The gate must never depend on link state, an assigned
address, or reachability from anywhere else. Ethernet may arrive over USB, which
enumerates seconds later than PCI, and a gate that waited for the network would roll
back a perfectly good update because a dongle was slow to appear — the
too-strict-health-gate failure ADR-0005 warns about. A machine with an unplugged cable
is a machine with a network problem, not a machine that needs its OS reverted.

## 3. No *separate* initramfs

An earlier draft of this document claimed MediaLith has no initramfs at all and that the
kernel execs `plexos-init` directly. That was wrong, and the correction matters enough
to state plainly rather than quietly edit.

Setting up dm-verity requires userspace before the root filesystem exists. The kernel
can create a verity device from the command line via `dm-mod.create`, but that forces
`/usr` itself to be the root, which forecloses the tmpfs root and `/etc` overlay the
rest of this design depends on. Something has to run first.

What MediaLith actually has is an initramfs **with no separate artifact**. A Unified
Kernel Image is a single PE binary with kernel, initrd, and command line in their own
sections, signed as a unit. So the property the earlier claim was reaching for still
holds exactly:

- there is no separate initramfs file to sign, verify, or keep in step with the kernel;
- the trust chain in [ADR-0004](adr/0004-verified-boot.md) is unchanged — one signature
  over one artifact;
- the initrd cannot be swapped independently of the kernel it shipped with.

`plexos-init` therefore runs twice, in two roles. First from the initrd: set up verity,
assemble the root at `/sysroot`, `switch_root`. Then from the verified `/usr` as the
service manager. `crates/plexos-init/src/plan.rs` computes the first role's work as a
plan before executing any of it, which makes the whole sequence testable and gives a
`--dry-run`.

The kernel still needs storage, erofs, and dm-verity built in rather than modular. The
initrd is deliberately minimal — one static binary — so there is no module loading, no
`udev`, and nothing to go stale.

That minimalism has a consequence worth stating, because it looks like an oddity
otherwise: `plexos-init` issues the device-mapper ioctls itself rather than calling
`veritysetup`, and creates `/dev/mapper/plexos-usr` itself rather than waiting for
`udev`. Neither tool is available at that point — `veritysetup` lives in `cryptsetup`,
inside the very `/usr` image being verified, and `udev` is exactly what "no module
loading, nothing to go stale" excludes. See [ADR-0011](adr/0011-syscall-boundary.md).

## 4. Update flow

```
  plexos-update                             update server
       |                                          |
       |-- fetch manifest + detached signature --> |
       |<------------------------------------------|
       |
       |  verify: signing cert chains to a baked-in root key,
       |          cert not expired, key not revoked,
       |          manifest sequence > last seen sequence   (anti-rollback)
       |
       |-- fetch payload (full image, or chunked delta) -->
       |
       |  verify payload SHA-256 against manifest
       |  write to inactive /usr slot + its verity slot
       |  write new UKI to ESP with try counter = 3
       |
       +--> reboot
```

The manifest schema can express chunked/delta sources from day one even though v0.1
only implements full-image downloads. Adding delta transport later must not require a
new manifest version, because devices in the field parse the manifest before they can
be told anything. See [ADR-0006](adr/0006-update-manifest.md).

Anti-rollback protection uses a monotonic `sequence` integer persisted in `/var`,
separate from the human-readable version. A signed old manifest replayed at a device
must be refused.

## 5. Plex as a separate payload

Plex Media Server is **not** part of the `/usr` image. It ships as its own read-only
image file under `/var/lib/plexos/apps/plex/`, mounted at runtime, versioned
independently of the OS.

Plex releases roughly weekly. If Plex lived in `/usr`, every Plex release would mean
downloading and rebooting into a whole new OS image. Separating them means a Plex
update is a download, an image swap, and a service restart — no reboot.

It also keeps the trust boundaries honest: the OS image is built by us and signed by
us; the Plex payload is built by Plex and cannot be redistributed by us at all
([ADR-0010](adr/0010-plex-provisioning.md)).

## 6. Hardware transcoding

This is the feature that justifies the project. The failure mode MediaLith exists to
eliminate is the one every Plex user knows: transcoding silently falls back to
software, the CPU pins at 100%, and playback stutters — with nothing in any log
saying why.

`plexos-gpu` runs at boot and on demand:

1. Enumerate DRM render nodes and identify the GPU.
2. Select and load the correct VA-API driver (`iHD` for Gen9+ Intel, `i965` for
   older, `radeonsi` for AMD).
3. Confirm the required firmware blobs (GuC/HuC on Intel) actually loaded — HuC is
   what makes QuickSync fast, and it silently fails to load often.
4. Run a real short transcode through the same code path Plex uses.
5. Report the result as a first-class system health state, visible in the setup UI.

A GPU that cannot transcode is a *reported failure*, not a silent degradation.

## 7. Configuration

A single declarative file, `/etc/plexos/config.toml`, carrying an explicit
`schema_version`. `plexosd` reads it and reconciles system state to match; it never
rewrites the file. Unknown keys within a known schema version are rejected, because
on an appliance a silently ignored typo is worse than a startup error.
See [ADR-0008](adr/0008-configuration-model.md).

## 8. Planned crates

| Crate | Status | Responsibility |
| --- | --- | --- |
| `plexos-types` | exists | Formats: partition contract, manifest, config schema, versions |
| `plexos-gpu` | exists | GPU detection, driver selection, transcode diagnosis |
| `plexos-sys` | exists | The only crate allowed `unsafe`: dm-verity ioctls, mount syscalls (ADR-0011) |
| `plexos-init` | partial | PID 1: plans and executes the boot; supervisor pending |
| `plexos-update` | planned | Manifest verification, download, slot write, rollback arming |
| `plexos-storage` | planned | Disk discovery, pools, SMART, snapshots |
| `plexos-shares` | planned | ksmbd and NFS export configuration |
| `plexosd` | planned | Management API, setup wizard, config reconciliation |
| `xtask` | planned | Image assembly, signing, QEMU test harness. Image assembly currently lives in `board/plexos/x86_64/post-image.sh` |

Crates are created when there is something real to put in them.

`plexos-types` came first because it is the only one whose mistakes cannot be corrected
in a later release — a judgement since vindicated, as two of its four partition type
GUIDs turned out to be wrong, and every test in the module passed anyway because they
all checked the layout against itself rather than against the published specification. `plexos-gpu` came second for the opposite reason: it is entirely
revisable, and it tests the premise everything else rests on. It runs standalone on any
Linux system, so the question "does QuickSync actually work on this box, and can we
tell when it does not" is answerable before an image exists to answer it on.

## 9. Known risks

**CVE maintenance.** The failure mode that ends small distributions. Mitigations: a
base of under 100 packages, Buildroot's `make pkg-stats` wired into CI, and a hard
rule that anything not required to run Plex does not go in the image.

**Plex packaging changes.** MediaLith depends on the shape of Plex's Debian package and
its glibc requirements. A breaking change upstream breaks provisioning. The app-image
boundary limits the blast radius to one component.

**NVIDIA.** The proprietary driver has redistribution restrictions and an
out-of-tree build. It is out of scope for v1; Intel iGPU is the target.

**Secure Boot key handling.** Shipping a distribution with Secure Boot means either
signing with a key users must enrol, or going through Microsoft's shim process. This
decision is deferred but must be made before the first public image.
