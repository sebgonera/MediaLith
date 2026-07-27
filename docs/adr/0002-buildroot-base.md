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
