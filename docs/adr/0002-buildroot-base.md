# ADR-0002: Buildroot as the base build system

**Status:** Accepted
**Date:** 2026-07-27

## Context

PlexOS needs a base userland: glibc, Mesa with VA-API, the Intel media driver,
firmware blobs, ksmbd tools, and little else. Something has to build that, reproducibly
and cross-compiled, and produce a filesystem image rather than a package set.

Plex Media Server is a closed-source binary linked against glibc. This rules out any
musl-based approach outright — Alpine and similar are not options regardless of their
other merits.

The project is small. Iteration speed is not a nice-to-have; a base system where a
change takes hours to test will not get changed, and the design will bend around the
build tool instead of around the problem.

## Decision

Buildroot, with PlexOS carried in a `BR2_EXTERNAL` tree under `buildroot/` so that
upstream Buildroot stays an unmodified pinned checkout.

Target for v1: `x86_64`, glibc, Intel iGPU (Gen9 and newer).

## Alternatives considered

**Yocto / OpenEmbedded.** The industrial standard for appliance Linux: layered
metadata, real BSP support across vendors, and mature OTA integrations (RAUC, Mender).
If PlexOS were shipping on five hardware platforms this would be the answer.

Rejected for now on iteration cost. BitBake builds are measured in hours, the metadata
model is large, and for a single fixed x86-64 target it buys capabilities we do not
use. The migration path stays open: the decisions that actually matter — partition
layout, trust chain, manifest format — are specified independently of the build system
(ADR-0003, ADR-0004, ADR-0006), so replacing Buildroot later changes how images are
produced without changing what an image *is*.

**mkosi with a Debian base.** By far the fastest start. Plex ships an official `.deb`,
and hardware enablement, firmware, and Mesa all work with no effort. Rejected as the
long-term base because the result is a Debian derivative carrying Debian's whole
package set and CVE surface — the opposite of the sub-100-package target that makes
maintenance survivable. It remains a useful throwaway tool for prototyping the Plex
runtime environment before the Buildroot base is ready.

**A custom rootfs built from source by our own Rust tooling.** Maximum control and
maximum purity with respect to "written in Rust". Rejected: bootstrapping and
maintaining a toolchain, glibc, and Mesa is a multi-year effort that produces no
Plex-specific value. The Rust content of this project is the userland we design, not
a from-scratch reimplementation of the one that already works.

## Consequences

- Hardware enablement is manual. Mesa, `intel-media-driver`, and `linux-firmware` need
  explicit Buildroot packages and configuration, and getting VA-API working is real
  work rather than an `apt install`. This is a one-time cost and it is exactly the cost
  that produces a small image.
- Buildroot has no runtime package manager, which fits ADR-0001 rather than fighting it.
- `make pkg-stats` gives a CVE report over the package set; this goes into CI as soon
  as there is a defconfig to run it against.
- Buildroot's own version must be pinned and updated deliberately, since it carries
  package versions and patches.
- The kernel configuration is ours to maintain. Storage, filesystem, and dm-verity
  drivers must be built in, not modular, because there is no initramfs.

## Revision note, 2026-08-12: the musl premise is contradicted by the artefact

**The decision stands. One of its stated reasons does not, and this records that
rather than acting on it.**

The Context above says:

> Plex Media Server is a closed-source binary linked against glibc. This rules out any
> musl-based approach outright — Alpine and similar are not options regardless of their
> other merits.

That was written before any Plex package had been unpacked and inspected. It is not
true of the package MediaLith actually installs. Plex Media Server
`1.43.3.10861-07dfddaeb`, fetched through the appliance's own catalogue URL
(`https://plex.tv/api/downloads/5.json`) and verified against the SHA-1 that catalogue
publishes, carries:

- `lib/ld-musl-x86_64.so.1` — the **musl** dynamic loader;
- `lib/libc.so`, byte-for-byte the same size as that loader, which is how musl ships;
- `lib/libgcompat.so.0`, the glibc-compatibility shim used on musl systems;
- 61 bundled libraries in total: its own `libc++`, OpenSSL, curl, ICU, Boost, FFmpeg
  and `libdrm`.

The main executable has **no `PT_INTERP` at all** and a `RUNPATH` of `$ORIGIN/lib`.
Traced with `strace -e trace=%file` while starting, the only paths it opens outside its
own directory tree are `/proc/self/exe` and the executable itself. It loads **nothing**
from the host — no glibc, and no library MediaLith's Buildroot userspace provides.

So "Plex is linked against the host glibc" is not a property of this release. Whether it
was ever true of an older one is unknown and was not investigated.

### What this does *not* change

Nothing, in this phase. The toolchain choice was deliberately left alone:
`BR2_TOOLCHAIN_BUILDROOT_GLIBC=y` is unchanged, no libc migration was attempted, and the
CPU-baseline work this note came out of is independent of it. A libc change would move
every binary in `/usr`, which is a dm-verity image referenced by a signed root hash and
subject to A/B rollback — a migration, not an edit.

### The follow-up this needs

A separate architecture audit, with its own ADR, answering:

> Does any MediaLith-owned Buildroot component still require glibc, and would a musl
> Buildroot userspace materially reduce size, attack surface, or compatibility risk
> without affecting Plex?

It needs its own clean build, binary inspection, image boot, update/rollback
compatibility analysis, and performance comparison. Two things are already known to be
worth checking first: the workspace's own binaries are built by host cargo for
`x86_64-unknown-linux-gnu` with `+crt-static` and link nothing from the Buildroot
sysroot, so they are indifferent to this; and `BR2_TOOLCHAIN_BUILDROOT_GLIBC` was
originally chosen partly because kconfig silently dropped the glibc selection and left
uClibc behind, which is a trap that will be waiting for whoever tries.
