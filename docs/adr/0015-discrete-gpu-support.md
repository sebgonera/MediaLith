# ADR-0015: Discrete GPUs, and what NVIDIA would cost

**Status:** Accepted
**Date:** 2026-07-29

## Context

PlexOS was moved to a machine with no integrated graphics and an RTX 5060 and did no
hardware transcoding. That is not a fault: the kernel binds nothing to the card, so
`/sys/class/drm` holds only `version` and `/dev/dri` never exists. The measurements
behind everything below were taken from the tree and from that machine rather than
recalled.

- `0000:01:00.0 class=0x030000 vendor=0x10de device=0x2d05 driver=NONE`
- **`# CONFIG_MODULES is not set`.** This kernel cannot load a module at all.
- `CONFIG_DRM_I915=y` and `CONFIG_DRM_XE=y`. `CONFIG_DRM_AMDGPU` is not set.
- `/usr` is 73.6 MiB; `/lib/firmware` inside it is 46 MiB of that.
- Buildroot's `nvidia-driver` package is pinned at **390.151**, a branch for Kepler-era
  cards. It is no use for anything made in the last decade.

The question this settles is not "should NVIDIA work". It is **what admitting an
out-of-tree driver does to an image whose defining property is that it is one verified
artefact**, and whether the answer is worth it for this hardware.

## The thing that actually costs

Everything in `/usr` today is built from source into a single erofs image, hashed as a
whole, and mounted read-only behind dm-verity (ADR-0004). Nothing is loaded later,
because there is nothing to load: the kernel is monolithic and there is no
`/lib/modules`. That is not an accident of configuration. It means the set of code that
can execute in kernel context is fixed at build time and covered by one root hash.

An NVIDIA driver cannot be built into the kernel. It is out-of-tree, so supporting it
means `CONFIG_MODULES=y`, a `/lib/modules` tree, and something that loads a module at
boot. The module still lives in `/usr` and is still covered by the same root hash, so
the *trust* property survives intact — but "the kernel is exactly what we built" becomes
"the kernel is what we built, plus whatever was inserted afterwards", and that is a
different sentence to have to defend.

`CONFIG_MODULE_SIG` closes most of the gap and should be set if this happens at all: the
kernel then refuses any module not signed by a key built into it, so an attacker who can
write to `/usr` cannot insert code — but they cannot anyway, because `/usr` is verity.
The honest summary is that module signing buys little *here* and costs nothing, so it
goes in as belt and braces.

## Decision

**Three steps, in this order, and the first two are worth doing whether or not the third
ever happens.**

### 1. `amdgpu`, built in

One kconfig symbol and its firmware. AMD's driver is in-tree, exposes a render node,
and works with the VA-API path this project already proves out end to end — the same
`plexos-gpu` probe, the same `vainfo` verification, no new trust story. It covers every
Radeon and every AMD APU, which is a large share of the hardware a person might put a
media server on.

The cost is size: AMD's firmware is the largest single family in `linux-firmware`. The
image grows, and that is the whole of it.

This is the cheapest hardware coverage available and it is not blocked on anything.

### 2. Report what the machine has, before it can use it

Done, in the commit this ADR accompanies. `plexos_gpu::display_devices` reads the PCI bus
so a card with no driver is told apart from no card at all. Any hardware decision a user
makes starts with the appliance saying what it sees, and until now it said the wrong
thing confidently.

### 3. NVIDIA, as an explicitly bounded piece of work

**Accepted as a goal, not scheduled.** It is feasible and it is not small. What it
requires, in the order the work would be done:

1. **`CONFIG_MODULES=y`, `CONFIG_MODULE_SIG=y`, a `/lib/modules` tree, and module loading
   at boot.** This is the architectural change and it is not about NVIDIA — it is the
   step that admits out-of-tree code at all. Everything else is packaging.
2. **A Buildroot package building NVIDIA's open kernel modules from source** against the
   pinned kernel. Blackwell requires the open module; the proprietary one does not
   support it. Dual MIT/GPL, so it fits a build-from-source image — this part is the
   *good* news and the reason the answer is not simply no.
3. **GSP firmware.** The open modules do not work without it. It is a redistributable
   blob and it is large. Unlike i915's GuC/HuC it does **not** need to be in the
   initramfs, because a module loads long after `do_initcalls` — the trap recorded for
   i915 does not apply here, and assuming it does would waste a build cycle.
4. **The proprietary userspace.** `libnvcuvid` and `libnvidia-encode` are what Plex
   actually calls; VA-API is not the path on NVIDIA. These are binary-only, and their
   licence permits redistribution under conditions that have to be read rather than
   assumed. This is the part that sits worst with a distribution built from source, and
   it is the part that cannot be engineered around.
5. **A version coupling that will bite.** The open modules build against a range of
   kernel versions. Every kernel bump becomes a bump that can fail to compile against a
   driver release, on a project whose update story is a whole-image replacement. That is
   a recurring maintenance cost, not a one-off.

## Alternatives considered

**Nouveau.** In-tree, no proprietary userspace, and it would fit this project perfectly.
Rejected on capability rather than principle: video decode on nouveau is not a path Plex
uses, and Blackwell support is too new to rely on. It is the right answer to a question
nobody is asking, because the thing wanted here is NVDEC and NVENC.

**A separate "driver pack" image.** A second verified artefact carrying out-of-tree
drivers, mounted beside `/usr`. It keeps the base image small and honest, and it is a
second update stream, a second signing story and a second rollback interaction — for one
family of cards. Rejected as disproportionate now; worth revisiting if a third
out-of-tree driver ever appears.

**Two image variants, with and without NVIDIA.** Rejected: an appliance should boot on
the hardware it is put on. A user choosing an image by what is inside their case is the
kind of decision this project exists to remove.

**Doing nothing and documenting it.** The status quo, and defensible: Intel and AMD
between them cover most of what a media server is built on, and `xe` already covers Arc.
Rejected as the *whole* answer because "buy different hardware" is not a remedy the owner
of an RTX 5060 can act on — but it is the honest short-term one, and the report now says
so by name.

## Consequences

- **Intel remains the proven path**, and `xe` is already built in, so current Arc parts
  work today with no change. That is the recommendation to anyone asking what to buy
  while step 3 is unscheduled.
- Step 1 changes what "the image is one verified artefact" means, and every later
  statement about the trust model has to be written with modules in mind.
- Step 3 puts a binary-only userspace in an image whose licence is still unchosen (open
  decision 3 in CLAUDE.md). Those two questions become one question.
- `plexos-gpu`'s probe-driven design already handles this: it picks a driver from what
  the kernel bound and verifies by probing. An NVIDIA path would be a new branch there,
  not a rewrite — the report would ask `nvidia-smi` or the encode library rather than
  `vainfo`.
- Until step 3, an NVIDIA machine transcodes on the CPU and the console says exactly why,
  naming the device and the missing driver.
