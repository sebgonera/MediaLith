# PlexOS

An immutable, atomically-updated Linux appliance distribution built to run Plex Media
Server well — and to say clearly what is wrong when it does not.

A general-purpose distribution running Plex has one characteristic failure: hardware
transcoding stops working and **nothing says so**. Plex falls back to software, the CPU
saturates, playback stutters, and the person who owns the machine is left guessing. Every
layer above the fault reports success, because every layer above the fault runs as root
and the fault is that Plex does not.

PlexOS is a whole system built around not doing that.

---

## Status

It runs. Specifics, because "it runs" is the kind of claim this project distrusts:

- **Boots from its own disk** on the reference laptop, `/usr` verified by dm-verity and
  mounted read-only, `/var` writable, `/etc` an overlay, `plexos-init` as PID 1.
- **Installs itself.** The installer is a mode of the running image and writes *the system
  currently running* to a chosen disk. It partitioned and booted a laptop's internal
  465 GiB drive that had Windows on it, in under two minutes.
- **Installs Plex from a browser** — downloads it from Plex's own endpoint, verifies the
  signature against a pinned key, builds an erofs app image, and starts it confined:
  cgroup v2, Landlock, uid 900.
- **Transcodes on the GPU.** 4K HDR10 HEVC Main 10 to 1080p HEVC, `(hw)` on both the
  decode and the encode — on Intel through VA-API, and on an RTX 5060 through NVDEC and
  NVENC.
- **Updates itself over the network** from a signed manifest, with an anti-rollback
  counter and root-signed key revocation. A replayed older release, correctly signed by
  the real key, is refused anyway — the one case no signature check can catch.
- **Undoes a bad update with nobody present.** A bundle whose `/usr` had a data block
  overwritten was installed and booted: three failed boots, then the machine came back on
  the previous release by itself. A second experiment, where the image booted fine but
  Plex could not start, ended the same way.

Seven crates, **827 tests**, and roughly 39 packages in the base image.

> ### The management console is for a trusted LAN
>
> It offers a root shell and the ability to replace the operating system. It is served
> over TLS with a self-signed certificate — there is no CA and no domain name — and every
> mutating request needs a device token. That stops anyone *listening*. Against an active
> middle it proves nothing until somebody compares the certificate fingerprint, and
> [ADR-0014](docs/adr/0014-console-terminal.md) states plainly that this is unresolved.
> Do not expose it to the internet.

---

## How it works

**One artefact, verified whole.** `/usr` is a read-only erofs image covered by a
dm-verity hash tree. The root hash travels in the kernel command line, inside a unified
kernel image, so the thing that checks the filesystem cannot be edited independently of
the filesystem.

**Two slots.** An update writes a complete new `/usr` to the inactive slot and installs a
boot entry on trial. The bootloader spends a try *to* boot it; a boot that reaches
userspace and passes a health gate makes the slot permanent, and one that does not falls
back. The counter is spent by booting, which is why a failed boot has to end in another
boot rather than in a panic screen — `panic=N` is on the command line for that reason and
a test asserts it.

**The frozen layout** ([ADR-0003](docs/adr/0003-partition-layout.md)), which reaches real
disks and can never be changed afterwards:

| # | Purpose | Size | Filesystem |
| --- | --- | --- | --- |
| 1 | ESP | 512 MiB | FAT32 |
| 2 | `usr_a` | 1 GiB | erofs, read-only |
| 3 | `usr_a` verity hashes | 32 MiB | dm-verity hash tree |
| 4 | `usr_b` | 1 GiB | erofs, read-only |
| 5 | `usr_b` verity hashes | 32 MiB | dm-verity hash tree |
| 6 | `var` | remainder | XFS |

**Rollback reverts `/usr` and never `/var`.** That rule is what makes updates safe and it
is also the sharpest constraint in the system: any migration must leave state the previous
release can still read, and a fault that lives on `/var` cannot be cured by rolling back
at all. Both halves have been demonstrated on hardware, the second one the hard way.

---

## The crates

Everything PlexOS owns is Rust. The kernel stays C, and so does the userland Plex needs —
Plex Media Server is a closed-source, dynamically linked glibc binary, and that constrains
the base.

| Crate | Role | Tests |
| --- | --- | --- |
| `plexos-sys` | The kernel-interface layer: verity, dm ioctls, mount, execve, Landlock, privilege dropping, PTYs, reaping | 95 |
| `plexos-init` | PID 1. Plans and executes the boot, then supervises what it started | 84 |
| `plexosd` | The console: HTTP server, status, installer, updater, settings, terminal, TLS | 357 |
| `plexos-plex` | Provisioning Plex from its own signed packages, and confining it | 106 |
| `plexos-update` | A/B updates, the signed trust chain, anti-rollback, revocation | 65 |
| `plexos-types` | Every on-disk and on-wire format, plus the GPT writer | 65 |
| `plexos-gpu` | Whether hardware transcoding works, and if not, what to do | 53 |

**`unsafe` is forbidden everywhere except `plexos-sys`**, which exists so that it can be.
PID 1 has to issue syscalls; confining them to one small crate keeps the unsafe
reviewable, and every block there carries a soundness comment enforced by a lint.

---

## `plexos-gpu` runs anywhere

It is useful on its own, on any Linux system, and it is the shortest way to see what this
project is about. If you are sizing up a machine as a Plex box, this answers whether its
hardware transcoding actually works:

```sh
cargo run -p plexos-gpu            # human-readable report
cargo run -p plexos-gpu -- --json  # for tooling

# exit status: 0 ready, 1 degraded, 2 unavailable
```

**Every finding names a remedy**, and a test enforces it. A report that says "hardware
acceleration unavailable" and stops has reproduced the problem this project exists to fix.

The rule earns its keep. Moving a USB stick between four machines in two days found three
defects that months of reading code had not: a discrete card with no driver bound, which
through `/sys/class/drm` is indistinguishable from having no graphics at all; a render
node at `0600 root:root`, invisible because every probe above it runs as root while Plex
does not; and GuC/HuC firmware shipped for exactly one chip generation, which transcoded
at reduced quality on a different laptop and reported success throughout.

---

## Hardware

| Machine | State |
| --- | --- |
| Core i5-8265U / UHD 620 (reference) | Full: boots, installs, provisions, transcodes on the iGPU |
| Alder Lake-P laptop | Transcodes on the iGPU |
| RTX 5060 desktop, no integrated graphics | Transcodes through NVDEC/NVENC, open modules 610.57.04 |
| Intel Arc | `CONFIG_DRM_XE=y` and firmware installed, **never tried — no hardware here** |
| AMD | Not built. Deliberately unscheduled |

**Secure Boot must be turned off.** Kernel images are self-signed and no keys are enrolled
([ADR-0004](docs/adr/0004-verified-boot.md)). This is separate from update signing, which
is done.

---

## Design principles

1. **Irreversible decisions first.** Partition GUIDs, the manifest schema, the config
   schema and the `/var` layout reach real devices and cannot be changed afterwards. They
   live in `plexos-types` and are treated as append-only.
2. **Rollback is not a feature, it is the default.** An update that boots badly must undo
   itself without a human present.
3. **Every diagnostic names a remedy.**
4. **Verify, don't recall.** Buildroot option names, kernel `CONFIG_*` symbols and PCI IDs
   are checked against the tree or a capture. Guessing there has cost real bugs.
5. **Report what is unverified.** Files that have never been built or executed say so at
   the top, and the notice is deleted only when the thing has actually run.
6. **A narrow package set.** CVE maintenance is what kills small distributions; the only
   defence is having little to maintain.

The most useful document here may be the "known traps" list in
[CLAUDE.md](CLAUDE.md) — every entry is a fault that reached hardware, written down in
the form that would have caught it. A recurring shape: *a control that is correct in the
state it was written in can be wrong in the state its own success produces.*

---

## What is not done

- **No licence has been chosen** — see below.
- **The name** uses a third-party trademark and will probably have to change.
- **Secure Boot keys** are not enrolled, so images are self-signed.
- **The root signing key is a development key.** Its private half sits on a build host,
  and every place that reports a signature says so.
- **Arc and AMD are unverified.** One has no hardware here; the other is not built.
- **No CI.** The suite runs on a build host, and images are tested by being put on
  machines.

## Licensing

Three separate things, none of them settled:

- **PlexOS's own code has no licence yet.** Until one is chosen, default copyright applies.
- **Plex Media Server is proprietary** and is not redistributed inside PlexOS images. It is
  fetched from Plex's own servers at provisioning time
  ([ADR-0010](docs/adr/0010-plex-provisioning.md)).
- **NVIDIA's userspace libraries are proprietary.** Their licence permits redistribution
  only if the binaries are unmodified *and* the agreement ships to each recipient. The
  kernel modules are the open ones, dual MIT/GPL. This repository contains the build
  recipe, not the binaries.

## Building

The Rust workspace builds anywhere:

```sh
cargo test --workspace
```

Image builds need a Linux host with real network access — Buildroot fetches from a dozen
hosts. See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Reading further

Start with [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), then
[docs/adr/](docs/adr/) for why anything is the way it is. Sixteen decision records, each
with the context that forced it.
