# PlexOS

A small, immutable, atomically-updated Linux appliance distribution built for a single
purpose: running Plex Media Server well.

**Status: design phase.** No bootable image exists yet. This repository currently
contains the architecture, the accepted design decisions, and the Rust crate that
encodes the on-disk and on-wire formats those decisions define.

## What this is

PlexOS is an appliance OS, not a general-purpose distribution. It boots from a
read-only, cryptographically verified `/usr` image, keeps all mutable state on a
single persistent partition, and updates by writing a whole new image to an inactive
slot and rebooting into it. A failed boot rolls back automatically.

The target hardware for v1 is an x86-64 mini-PC with an Intel iGPU — the class of
machine most Plex servers actually run on today, where QuickSync handles several
concurrent 4K transcodes inside a 15 W power envelope.

## What is written in Rust

The kernel stays C (mainline, trimmed). glibc, Mesa/VA-API, and FFmpeg stay C —
Plex Media Server is a closed-source, dynamically linked glibc binary, and that
constrains the base userland.

Everything we own is Rust:

| Component | Role |
| --- | --- |
| `plexos-init` | PID 1: mounts, dm-verity setup, `/etc` overlay, service supervision, cgroup v2 |
| `plexos-update` | A/B updater: manifest verification, download, slot write, rollback arming |
| `plexos-gpu` | GPU detection and a boot-time hardware transcode self-test |
| `plexos-storage` | Disk discovery, pool assembly, SMART monitoring, snapshots |
| `plexos-shares` | SMB (ksmbd) and NFS export management |
| `plexosd` | Management API and setup wizard; applies declarative config to system state |
| `plexos-types` | Shared definitions of every on-disk and on-wire format |

`plexos-types` and `plexos-gpu` exist today. The first pins the decisions we cannot
revise later; the second tests the premise the whole project rests on.

## Repository layout

```
docs/ARCHITECTURE.md   System design and component responsibilities
docs/adr/              Accepted architecture decision records
crates/plexos-types/   On-disk and on-wire format definitions
crates/plexos-gpu/     GPU detection and hardware transcode diagnosis
buildroot/             BR2_EXTERNAL tree (skeleton)
```

## Checking your hardware today

`plexos-gpu` runs standalone on any Linux system, not just on PlexOS. If you are
considering a machine as a Plex box, this answers whether its hardware transcoding
actually works — and if not, what to do about it:

```
cargo run -p plexos-gpu           # human-readable report
cargo run -p plexos-gpu -- --json # for tooling

# exit status: 0 ready, 1 degraded, 2 unavailable
```

It needs `vainfo` (from `libva-utils`) to reach its most useful conclusions.

## Design principles

1. **Irreversible decisions first.** Partition GUIDs, the update manifest schema, and
   the config schema ship to real devices and cannot be changed afterwards. They are
   specified, versioned, and covered by round-trip tests before any code that uses
   them is written.
2. **Every format carries a version field.** A device running v0.1 must be able to
   parse enough of a v3 manifest to report *why* it cannot apply it.
3. **Rollback is not a feature, it is the default.** An update that boots badly must
   undo itself without a human present.
4. **Narrow package set.** Fewer than 100 packages in the base image. CVE maintenance
   is the failure mode that kills small distributions; the only defence is having
   little to maintain.

## Licensing note

Plex Media Server is proprietary and cannot be redistributed inside PlexOS images.
It is provisioned on first boot from Plex's own servers. See
[ADR-0010](docs/adr/0010-plex-provisioning.md).

The license for PlexOS's own code has not been chosen yet.

## Building

Nothing to build yet beyond the Rust workspace:

```
cargo test --workspace
```
