# ADR-0015: Discrete GPUs, and what NVIDIA would cost

**Status:** Accepted
**Date:** 2026-07-29

## Revision — 2026-08-12: step 1 is done, and it cost more than the one symbol

The Context below is left exactly as it was measured on 2026-07-29, including the line
`# CONFIG_MODULES is not set`, which stopped being true when step 1 was taken. A dated
record is worth more than one edited to stay current, and this note is where the current
state goes.

`CONFIG_MODULES=y` now, with `MODULE_SIG_FORCE` beside it, and `plexos-init` loads four
NVIDIA modules by `finit_module` — there is still no udev, no kmod and no modprobe, so
those four are loaded because they are named in `plexos_init::nvidia::MODULES` and nothing
else can be.

What this decision did not anticipate is the part worth recording, because it is a general
consequence of the trade rather than anything to do with NVIDIA. **Turning `MODULES` on
gives kconfig a third answer for every tristate symbol in the tree, and it takes it.**
Eleven options that had been built in or absent became `=m` on the next build, producing
eight `.ko` files — and in an image with no module loader, a module is a feature that is
silently gone. It compiles, it installs, it passes every test, and the thing it does never
happens on a machine.

One of the eight was `x86_pkg_temp_thermal`, which publishes the only thermal zone that
reports the processor die. The activity card fell back to `acpitz` — a chassis sensor —
and reported it as the processor, with nothing failing and nothing logged. The other seven
were netfilter extensions that nothing in this image could ever have reached.

Two things follow, and both are in place:

- Anything MediaLith depends on is pinned `=y` in `linux.fragment`, and anything it does not
  want is pinned `=n` rather than left modular.
- `post-image-test.sh` stage 7 asserts that **every shipped `.ko` is one the loader names**,
  reading the list out of `nvidia.rs` rather than keeping a second copy. A future kernel
  bump that turns a built-in subsystem into a module fails the build and prints the module.

The general form of it belongs with the decision: **admitting modules to this image changed
the default answer for every driver in the kernel, not only for the one that motivated it.**

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

**Report first, then NVIDIA. `amdgpu` stays available and unscheduled.**

### `amdgpu`, deliberately not first

One kconfig symbol and its firmware, in-tree, exposing a render node through the same
VA-API path this project has already proven end to end — no new trust story at all. It
is by far the cheapest hardware coverage available, and it was the obvious first step
until the obvious was checked against who is actually running this: none of the
hardware here is AMD. Doing it first would be widening support for machines nobody
owns while the machine somebody owns still transcodes on its CPU.

It stays written down because the reasoning survives a change of owner, and because the
day a second person runs this it is an afternoon's work.

### 1. Report what the machine has, before it can use it

Done, in the commit this ADR accompanies. `plexos_gpu::display_devices` reads the PCI bus
so a card with no driver is told apart from no card at all. Any hardware decision a user
makes starts with the appliance saying what it sees, and until now it said the wrong
thing confidently.

### 2. NVIDIA, as an explicitly bounded piece of work

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

## What step 2 actually involves

Written out because "add NVIDIA support" hides its shape. Every number here was checked
rather than recalled, and the order is chosen so the things most likely to stop the work
are discovered first rather than last.

**The size question is settled and it is not a problem.** `usr_a` is 2097152 sectors —
1 GiB — and today's image uses 73.6 MiB of it. NVIDIA's userspace does not come close to
filling the remainder, so none of this touches ADR-0003 or the frozen layout.

**The hardware is supported.** NVIDIA's open kernel modules cover Turing and later,
which includes this card, and the current release is 610.43.03. They state a minimum of
Linux 4.15 and no maximum — which is a claim about their intent, not a test against
6.19, and the first build is what turns one into the other.

### In order

1. **`CONFIG_MODULES=y`, and admit it is a reversal.** It is set to `n` in
   `linux.fragment` under "Trimming", beside `DRM_NOUVEAU` and `DRM_AMDGPU`, with the
   reasoning "each is attack surface plus CVE maintenance". That reasoning is still
   true; it is simply outweighed if this hardware is to work. `CONFIG_MODULE_SIG=y` and
   `CONFIG_MODULE_SIG_FORCE=y` go in at the same time: `/usr` is already verity, so this
   buys little, but a kernel that will load only what we signed costs nothing and closes
   the sentence properly. Build and boot this alone, changing nothing else — a monolithic
   kernel that suddenly has a module loader is a change worth isolating from a driver.

2. **Find out how `/dev/nvidia*` comes to exist, before writing a package.** This is the
   step most likely to be the expensive surprise, and it is nearly free to check. There
   is no `udev` here — the trap list has three separate things that assumed there was —
   and NVIDIA's device nodes are conventionally created either by udev rules or by
   `nvidia-modprobe`, a setuid helper this image does not have. `devtmpfs` creates nodes
   only for drivers that register through the device model. If the open modules do not,
   something in PlexOS has to create `/dev/nvidiactl`, `/dev/nvidia0` and
   `/dev/nvidia-uvm` with the right major and minor, and that belongs in `plexos-init`'s
   plan beside the other things it makes from nothing. Answer this with a throwaway build
   and a shell before packaging anything.

   **Answered, 2026-08-10, against release 610.57.04 — and it needed no hardware.** The
   question was decidable by reading the driver rather than by booting it, which is worth
   noting on its own: the throwaway build this step asks for was not required to get the
   answer, only to confirm it later.

   **`devtmpfs` will not create the nodes.** `kernel-open/nvidia/nv.c` registers with
   `register_chrdev_region` and calls `class_create` and `device_create` exactly zero
   times. The only device-model registrations anywhere in the tree are `nv-caps-imex` and
   `nvswitch`, neither of which is a graphics card. So the suspicion above is confirmed:
   something in PlexOS creates these nodes or nothing does.

   **The good news is that no setuid helper is needed.** The numbers are compile-time
   constants in `kernel-open/common/inc/nv-chardev-numbers.h`:

   | Node | Device | Source |
   | --- | --- | --- |
   | `/dev/nvidia0` | `c 195 0` | `NV_MAJOR_DEVICE_NUMBER`, minor per card from 0 |
   | `/dev/nvidiactl` | `c 195 255` | `NV_MINOR_DEVICE_NUMBER_CONTROL_DEVICE` |
   | `/dev/nvidia-modeset` | `c 195 254` | `NV_MINOR_DEVICE_NUMBER_MODESET_DEVICE` |

   `NV_MINOR_DEVICE_NUMBER_REGULAR_MAX` is 247, so a machine may have up to 248 cards
   before the regular minors collide with the special ones. Nothing here will.

   **`nvidia-uvm` is the exception, and it is the one that cannot be hard-coded.**
   `kernel-open/nvidia-uvm/uvm.c` uses `alloc_chrdev_region`, so the kernel assigns its
   major *at module load*. It has to be read back from `/proc/devices` after loading and
   before the node is made. That is the whole of the difficulty this step was expecting to
   find, and it is perhaps fifteen lines rather than a setuid binary this image would have
   refused to carry.

   **Permissions matter more than they look**, and the trap list already has this in
   another form: with no `udev`, a node created here is `0600 root:root`, and Plex does not
   run as root. `/dev/dri/renderD*` had to be relaxed to `0666` for exactly this reason,
   and the failure was invisible — every probe above it ran as root and reported success
   while Plex used the CPU. These nodes need the same treatment and the same test:
   `su -s /bin/sh -c … plex`, not a check that runs as root.

   **The kernel-version risk is softened but not closed.** The README claims 4.15 or newer
   with no upper bound, and there is no `LINUX_VERSION_CODE` guard in the sources that
   refuses a newer kernel outright — portability is handled by `conftest.sh`, which probes
   the API by trial compilation. So 6.19 is not rejected in advance. It is still a claim
   rather than a test, and step 3 is where it becomes one.

3. **A Buildroot package: `plexos-nvidia`.** Prefixed, because a package directory name
   becomes its kconfig symbol and colliding with upstream's `nvidia-driver` would have
   kconfig merge the two definitions silently — a trap already recorded here. It builds
   the open modules from source against the pinned kernel using Buildroot's
   `pkg-kernel-module` infrastructure, which is present in the tree and has working
   examples. Upstream's own `nvidia-driver` package is pinned at 390.151 and is not a
   starting point for this.

4. **GSP firmware, in `/usr`, not the initramfs.** The open modules do not run without
   it. The i915 trap does **not** apply: that firmware had to be in the initramfs because
   `i915` is built in and fetches during `do_initcalls`, a second before `/usr` is
   mounted. A module loaded after `/usr` is up can read from it, so the blob goes in the
   image like any other file. Assuming otherwise would cost a build cycle for nothing.

5. **The userspace, and only the parts Plex calls.** `libnvcuvid` and
   `libnvidia-encode`, plus the CUDA driver library they sit on. Not OpenGL, not Vulkan,
   not the X drivers — this machine has no display server and never will. These are
   binary-only, which is the part that does not fit a build-from-source image and cannot
   be engineered around.

6. **A Landlock grant for the device nodes.** Plex reaches the GPU through them, and the
   policy is deny-by-default. `/dev/dri` is already granted with `IOCTL_DEV` for exactly
   this reason on Intel; `/dev/nvidia*` needs the same. Three separate outages in this
   project have been a Landlock policy missing something nobody listed, so this is
   written down before it is discovered.

7. **A probe branch in `plexos-gpu`.** Its design is probe-driven rather than
   table-driven, which was the right call and pays here: the report asks `vainfo` what
   the hardware can do. NVIDIA is not a VA-API path, so it needs a different question —
   the encode library's own capability query, or `nvidia-smi` if the package ships it.
   The `Report` shape does not change; one more way of answering "what can this decode
   and encode" does.

8. **Module loading at boot, and the machine that has no NVIDIA card.** Most machines
   running this image will not have one. Loading must be conditional on the PCI device
   being present — which `plexos_gpu::display_devices` now reports — and its absence must
   be silent rather than an error on every boot of the reference laptop.

### What would make this stop

- The open modules failing to build against 6.19. Their claim of no upper bound is not a
  test, and every future kernel bump repeats the question.
- Device nodes needing a setuid helper this image will not carry, with no workable way to
  create them from `plexos-init`.
- The userspace licence turning out to forbid the redistribution this would require,
  which lands on the same unanswered question as the project's own unchosen licence.

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
  while this is unfinished.
- Step 1 changes what "the image is one verified artefact" means, and every later
  statement about the trust model has to be written with modules in mind.
- The last step puts a binary-only userspace in an image whose licence is still unchosen (open
  decision 3 in CLAUDE.md). Those two questions become one question.
- `plexos-gpu`'s probe-driven design already handles this: it picks a driver from what
  the kernel bound and verifies by probing. An NVIDIA path would be a new branch there,
  not a rewrite — the report would ask `nvidia-smi` or the encode library rather than
  `vainfo`.
- Until it is done, an NVIDIA machine transcodes on the CPU and the console says exactly why,
  naming the device and the missing driver.
