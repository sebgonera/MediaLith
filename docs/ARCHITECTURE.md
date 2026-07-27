# PlexOS Architecture

This document describes the system as designed. Individual decisions, with their
alternatives and consequences, live in [`docs/adr/`](adr/).

## 1. Shape of the system

PlexOS is an **image-based appliance OS**. It has no package manager on the device
and no in-place upgrade path. The unit of change is a whole `/usr` image, written to
whichever of two slots is not currently running, and activated by a reboot.

```
                    +-------------------------------------------+
   Secure Boot ---> | UKI (signed): kernel + initramfs-less      |
                    |               plexos-init + cmdline        |
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
3. The kernel starts `plexos-init` as PID 1 directly — there is no initramfs.
4. `plexos-init` sets up dm-verity over the `/usr` partition named on the command
   line, mounts it, assembles the root tmpfs, mounts `/var`, and overlays `/etc`.
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
accepting a request.

## 3. Why no initramfs

An initramfs exists to find and mount the real root. PlexOS already knows where its
root is — the partition is identified by a Discoverable Partitions Specification type
GUID, and the verity root hash arrives on the command line. Skipping the initramfs
removes an entire signed artifact from the trust chain and several hundred
milliseconds from boot.

The cost is that the kernel must have the storage and dm-verity drivers built in
rather than as modules. For a fixed appliance target that is acceptable, and it is
revisited if PlexOS ever supports arbitrary hardware.

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

This is the feature that justifies the project. The failure mode PlexOS exists to
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
| `plexos-init` | planned | PID 1, verity/mount setup, supervisor, health gate |
| `plexos-update` | planned | Manifest verification, download, slot write, rollback arming |
| `plexos-storage` | planned | Disk discovery, pools, SMART, snapshots |
| `plexos-shares` | planned | ksmbd and NFS export configuration |
| `plexosd` | planned | Management API, setup wizard, config reconciliation |
| `xtask` | planned | Image assembly, signing, QEMU test harness |

Crates are created when there is something real to put in them.

`plexos-types` came first because it is the only one whose mistakes cannot be corrected
in a later release. `plexos-gpu` came second for the opposite reason: it is entirely
revisable, and it tests the premise everything else rests on. It runs standalone on any
Linux system, so the question "does QuickSync actually work on this box, and can we
tell when it does not" is answerable before an image exists to answer it on.

## 9. Known risks

**CVE maintenance.** The failure mode that ends small distributions. Mitigations: a
base of under 100 packages, Buildroot's `make pkg-stats` wired into CI, and a hard
rule that anything not required to run Plex does not go in the image.

**Plex packaging changes.** PlexOS depends on the shape of Plex's Debian package and
its glibc requirements. A breaking change upstream breaks provisioning. The app-image
boundary limits the blast radius to one component.

**NVIDIA.** The proprietary driver has redistribution restrictions and an
out-of-tree build. It is out of scope for v1; Intel iGPU is the target.

**Secure Boot key handling.** Shipping a distribution with Secure Boot means either
signing with a key users must enrol, or going through Microsoft's shim process. This
decision is deferred but must be made before the first public image.
