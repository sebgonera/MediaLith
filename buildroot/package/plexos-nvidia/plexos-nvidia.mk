################################################################################
#
# plexos-nvidia
#
# NVIDIA's open GPU kernel modules, built against the kernel this image ships.
# ADR-0015 step 3.
#
# ---------------------------------------------------------------------------
# What has been proven, and what has not
# ---------------------------------------------------------------------------
#
# The compilation has been done by hand, with this toolchain, against this kernel,
# before this file existed -- because the ADR named it as one of the two things that
# could stop the whole exercise, and "4.15 or newer, no maximum" is a claim rather
# than a test. It is a test now:
#
#   610.57.04 against 6.19.14, EXIT=0, five modules, vermagic
#   "6.19.14 SMP preempt mod_unload" -- the kernel this image actually boots.
#
# Buildroot has since driven it too: four modules installed and signed, checked by the
# hooks below. What has NOT happened is any of it running on NVIDIA hardware -- the
# RTX 5060 does not have PlexOS on it -- and that notice stays until it does.
#
# ---------------------------------------------------------------------------
# Signing is not optional here
# ---------------------------------------------------------------------------
#
# The fragment sets CONFIG_MODULE_SIG_FORCE=y, so an unsigned module is refused at
# load. Modules come out of NVIDIA's build unsigned -- checked, they end without the
# "~Module signature appended~" marker -- and the failure mode if that is missed is
# the worst kind: the package builds perfectly, the image builds perfectly, and the
# card does nothing on the machine, which reads as a hardware fault.
#
# `make modules_install` would sign them, since the fragment also sets
# CONFIG_MODULE_SIG_ALL=y -- but this package does not go through modules_install, for
# the reasons below. So signing is done here explicitly with scripts/sign-file, which
# has been verified to work against this kernel's key, and then *checked* by a hook
# that fails the build if any module reached the target without the marker. Explicit
# and checked, rather than inherited from an infrastructure step nobody looked at.
#
# ---------------------------------------------------------------------------
# Not the whole driver
# ---------------------------------------------------------------------------
#
# GSP firmware is requested at runtime and is not in the source tarball, so it comes
# from a second download -- see the section on it below. That is ADR-0015 step 4 and it
# is now part of this package.
#
# What is still missing is the proprietary userspace: libnvcuvid and libnvidia-encode,
# which are how Plex reaches NVDEC and NVENC. Without them this image can bind the card
# and not transcode on it. Step 5, and the one that meets the project's unchosen
# licence rather than NVIDIA's.
#
# The device nodes are step 2's finding and belong in plexos-init's plan, not in a
# package: this driver registers with register_chrdev_region and never calls
# class_create, so devtmpfs creates nothing. Major 195, minor 255 for nvidiactl, 254
# for nvidia-modeset, one per card from 0 -- and nvidia-uvm's major is allocated at
# load time and has to be read back from /proc/devices.
#
################################################################################

PLEXOS_NVIDIA_VERSION = 610.57.04
PLEXOS_NVIDIA_SITE = $(call github,NVIDIA,open-gpu-kernel-modules,$(PLEXOS_NVIDIA_VERSION))

# Dual MIT/GPL-2.0, which is the reason this variant is usable at all: the
# proprietary modules could not be built from source into a verity-sealed image.
PLEXOS_NVIDIA_LICENSE = MIT OR GPL-2.0
PLEXOS_NVIDIA_LICENSE_FILES = COPYING

# ---------------------------------------------------------------------------
# Why this does not use Buildroot's kernel-module infrastructure
# ---------------------------------------------------------------------------
#
# It was written that way first, and the failure is worth keeping because it is not
# obvious from anything either project documents.
#
# `kernel-module` builds an external module the standard way: `make -C $(LINUX_DIR)
# M=<pkg>/kernel-open modules`. That is correct for an ordinary out-of-tree module and
# wrong for this one, in two stages:
#
#   1. Nothing built at all. kernel-open/Kbuild fills `obj-m` by iterating
#      NV_KERNEL_MODULES, which NVIDIA's own Makefile computes with `$(wildcard ...)`.
#      Calling the kernel directly skips that Makefile, so the list is empty -- and an
#      empty obj-m is not an error. The whole build was one line, `MODPOST
#      Module.symvers`, and it succeeded.
#
#   2. Passing the list by hand got further and then hit the real shape of the thing:
#
#          No rule to make target 'nvidia/nv-kernel.o_binary'
#
#      `nv-kernel.o_binary` is the OS-agnostic core, compiled out of src/nvidia by
#      NVIDIA's top-level Makefile *before* the kernel-open glue is built against it.
#      `M=kernel-open` enters at the second stage, so the first one never happens and
#      the link has nothing to link.
#
# So this drives their Makefile, which is the interface they support and the one the
# hand build used -- and the hand build is the only evidence this compiles against
# 6.19 at all. Installing and signing then have to be done here, which is no loss: with
# MODULE_SIG_FORCE, signing is too important to inherit from an infrastructure hook
# nobody checked.
PLEXOS_NVIDIA_DEPENDENCIES = linux

# ---------------------------------------------------------------------------
# GSP firmware (ADR-0015 step 4), and why it comes from a second download
# ---------------------------------------------------------------------------
#
# The open modules do not run without it. nvidia.ko requests it by name at runtime --
# the strings are in the built module -- and it is not in the source tarball:
#
#     nvidia/610.57.04/gsp_ga10x.bin      81 MB   Ampere onwards, Blackwell included
#     nvidia/610.57.04/gsp_tu10x.bin      29 MB   Turing
#     nvidia/610.57.04/ucodes_ga10x.bin   31 KB
#     nvidia/610.57.04/ucodes_tu10x.bin   12 KB
#
# Both families are carried. The alternative is an image that works on the card the
# developer happened to own, which is the mistake the GuC/HuC firmware list already
# made once and cost an evening to find. 110 MB is a real price and it is paid in
# /usr -- a 1 GiB partition using 126 MiB before this -- not in the initramfs. Unlike
# i915, this driver is a module loaded long after /usr is mounted, so the rule that
# forces GuC/HuC into the initrd does not apply here.
#
# gsp_log_*.bin is also requested and deliberately absent: osinit.c calls its absence
# non-fatal and it only silences GSP-RM logging.
#
# ---------------------------------------------------------------------------
# The licence permits this, with two conditions, and one of them is code
# ---------------------------------------------------------------------------
#
# NVIDIA Driver License Agreement 1.1(d) permits distributing the SOFTWARE for use
# with an OS kernel under an OSI-approved licence -- Linux is GPL-2.0, so that holds --
# "provided that (i) the binary files thereof are not modified in any way (except for
# uncompressing of compressed files) and (ii) this Agreement is provided to each
# SOFTWARE recipient."
#
# (i) is satisfied by copying the blobs untouched. (ii) is an obligation on the image
# rather than on this file, so the agreement is installed beside the firmware where a
# recipient can actually read it. An appliance that ships the firmware and not the
# licence has not met the condition it is relying on.
#
# Note this is separate from the project's own unchosen licence. The clause is about
# the *kernel's* licence, not the distribution's.
PLEXOS_NVIDIA_RUN = NVIDIA-Linux-x86_64-$(PLEXOS_NVIDIA_VERSION).run
PLEXOS_NVIDIA_EXTRA_DOWNLOADS = \
	https://us.download.nvidia.com/XFree86/Linux-x86_64/$(PLEXOS_NVIDIA_VERSION)/$(PLEXOS_NVIDIA_RUN)

PLEXOS_NVIDIA_FWDIR = /usr/lib/firmware/nvidia/$(PLEXOS_NVIDIA_VERSION)
PLEXOS_NVIDIA_FIRMWARE = gsp_ga10x.bin gsp_tu10x.bin ucodes_ga10x.bin ucodes_tu10x.bin

# ---------------------------------------------------------------------------
# The userspace Plex reaches NVDEC and NVENC through (ADR-0015 step 5)
# ---------------------------------------------------------------------------
#
# The whole installer carries 776 MB of libraries -- OpenGL, Vulkan, X11, EGL, the
# CUDA compiler. A headless transcoding appliance uses almost none of it, and shipping
# it all would be the firmware mistake in a larger size. What is here is the path Plex
# actually walks, established by reading each library's NEEDED entries rather than by
# taking a list from somewhere:
#
#   libcuda                 108 MB  the driver API; everything else sits on it
#   libnvcuvid               27 MB  NVDEC
#   libnvidia-encode        284 KB  NVENC, and it NEEDs libnvcuvid
#   libnvidia-ml            2.6 MB  NVML, which is what nvidia-smi asks
#   libnvidia-ptxjitcompiler 36 MB  dlopened by libcuda
#
# libnvidia-nvvm is deliberately absent, and it is the one judgement call here. libcuda
# names it alongside ptxjitcompiler, but it is the NVRTC runtime compiler -- for
# programs that compile CUDA source at run time, which decoding and encoding a video
# stream does not do. It is 75 MB. If a future Plex does something that needs it, the
# symptom will be a dlopen failure naming the file, which is at least a message that
# points here.
#
# nvidia-smi comes too, at 1.3 MB. It is the only way to ask this hardware anything from
# a shell, and this project's whole argument is that a diagnostic which cannot be run is
# a diagnostic nobody has.
#
# Licensing is the same clause as the firmware: 1.1(d) covers "the SOFTWARE", so the
# libraries travel under the same two conditions, unmodified and with the agreement
# beside them. Both are already enforced.
PLEXOS_NVIDIA_LIBS = \
	libcuda.so \
	libnvcuvid.so \
	libnvidia-encode.so \
	libnvidia-ml.so \
	libnvidia-ptxjitcompiler.so

# The SONAME each library is opened by. A file on disk that nothing can find by the name
# in its consumer's NEEDED entry is a file that is not there, and the failure reads as a
# missing driver.
PLEXOS_NVIDIA_SONAME_libcuda = libcuda.so.1
PLEXOS_NVIDIA_SONAME_libnvcuvid = libnvcuvid.so.1
PLEXOS_NVIDIA_SONAME_libnvidia-encode = libnvidia-encode.so.1
PLEXOS_NVIDIA_SONAME_libnvidia-ml = libnvidia-ml.so.1
PLEXOS_NVIDIA_SONAME_libnvidia-ptxjitcompiler = libnvidia-ptxjitcompiler.so.1

# The .run is a makeself archive: --extract-only is plain shell, tar and xz, so it does
# not execute any of the payload and does not care what architecture the build host is.
define PLEXOS_NVIDIA_EXTRACT_RUN
	rm -rf $(@D)/.run-extracted
	cd $(@D) && cp $(PLEXOS_NVIDIA_DL_DIR)/$(PLEXOS_NVIDIA_RUN) . && \
		sh ./$(PLEXOS_NVIDIA_RUN) --extract-only --target .run-extracted >/dev/null && \
		rm -f ./$(PLEXOS_NVIDIA_RUN)
endef

PLEXOS_NVIDIA_PRE_BUILD_HOOKS += PLEXOS_NVIDIA_EXTRACT_RUN


# The module list, written out rather than globbed: a module appearing upstream is then
# a deliberate decision here rather than something that arrives in an image because a
# wildcard matched it. nvidia-peermem is left out -- RDMA between a GPU and an
# InfiniBand adapter, which is a datacentre arrangement this appliance will never call.
PLEXOS_NVIDIA_MODULES = nvidia nvidia-uvm nvidia-modeset nvidia-drm

# Where a module that is not part of the kernel belongs.
PLEXOS_NVIDIA_MODDIR = /usr/lib/modules/$(LINUX_VERSION_PROBED)/extra

define PLEXOS_NVIDIA_BUILD_CMDS
	$(TARGET_MAKE_ENV) $(MAKE) -C $(@D) modules \
		SYSSRC="$(LINUX_DIR)" \
		CC="$(TARGET_CC)" \
		LD="$(TARGET_LD)" \
		AR="$(TARGET_AR)" \
		OBJDUMP="$(TARGET_OBJDUMP)" \
		ARCH=$(KERNEL_ARCH) \
		NV_EXCLUDE_KERNEL_MODULES="nvidia-peermem"
endef

# NV_VERBOSE=1 prints the full command line for every one of several thousand source
# files, each about two and a half kilobytes wide. It is the right thing to add by hand
# when something here needs diagnosing and the wrong thing to leave on: a build log
# nobody can read is a build log nobody reads.

# Signed here, explicitly, with the key the kernel generated. MODULE_SIG_FORCE refuses
# anything else, and a module that reaches the image unsigned produces a card that does
# nothing on the machine with nothing anywhere saying why.
define PLEXOS_NVIDIA_INSTALL_TARGET_CMDS
	$(INSTALL) -d -m 0755 $(TARGET_DIR)$(PLEXOS_NVIDIA_MODDIR)
	for module in $(PLEXOS_NVIDIA_MODULES); do \
		$(INSTALL) -m 0644 -D $(@D)/kernel-open/$$module.ko \
			$(TARGET_DIR)$(PLEXOS_NVIDIA_MODDIR)/$$module.ko || exit 1; \
		$(LINUX_DIR)/scripts/sign-file sha256 \
			$(LINUX_DIR)/certs/signing_key.pem \
			$(LINUX_DIR)/certs/signing_key.x509 \
			$(TARGET_DIR)$(PLEXOS_NVIDIA_MODDIR)/$$module.ko || exit 1; \
	done
endef

define PLEXOS_NVIDIA_INSTALL_FIRMWARE
	$(INSTALL) -d -m 0755 $(TARGET_DIR)$(PLEXOS_NVIDIA_FWDIR)
	for blob in $(PLEXOS_NVIDIA_FIRMWARE); do \
		$(INSTALL) -m 0644 -D $(@D)/.run-extracted/firmware/$$blob \
			$(TARGET_DIR)$(PLEXOS_NVIDIA_FWDIR)/$$blob || exit 1; \
	done
	$(INSTALL) -m 0644 -D $(@D)/.run-extracted/LICENSE \
		$(TARGET_DIR)/usr/share/licenses/nvidia/LICENSE
	$(INSTALL) -d -m 0755 $(TARGET_DIR)/usr/lib
	for lib in $(PLEXOS_NVIDIA_LIBS); do \
		src=$(@D)/.run-extracted/$$lib.$(PLEXOS_NVIDIA_VERSION); \
		test -s $$src || { echo "plexos-nvidia: $$src is not in the installer"; exit 1; }; \
		$(INSTALL) -m 0644 -D $$src \
			$(TARGET_DIR)/usr/lib/$$lib.$(PLEXOS_NVIDIA_VERSION) || exit 1; \
		soname=$$(cd $(@D)/.run-extracted && objdump -p $$lib.$(PLEXOS_NVIDIA_VERSION) 2>/dev/null \
			| sed -n 's/^ *SONAME *//p' | head -1); \
		test -n "$$soname" || soname=$$lib.1; \
		ln -sf $$lib.$(PLEXOS_NVIDIA_VERSION) $(TARGET_DIR)/usr/lib/$$soname; \
		ln -sf $$soname $(TARGET_DIR)/usr/lib/$$lib; \
	done
	$(INSTALL) -m 0755 -D $(@D)/.run-extracted/nvidia-smi $(TARGET_DIR)/usr/bin/nvidia-smi
endef

PLEXOS_NVIDIA_POST_INSTALL_TARGET_HOOKS += PLEXOS_NVIDIA_INSTALL_FIRMWARE

# The firmware is as necessary as the modules and absent for different reasons, so it
# gets its own check rather than sharing one. A module that loads and then finds no
# firmware leaves a card that does nothing, which is the same symptom as an unsigned
# module and a different cause -- worth telling apart before somebody is looking at
# hardware wondering which it is.
define PLEXOS_NVIDIA_CHECK_FIRMWARE
	@for blob in $(PLEXOS_NVIDIA_FIRMWARE); do \
		test -s $(TARGET_DIR)$(PLEXOS_NVIDIA_FWDIR)/$$blob || { \
			echo "plexos-nvidia: $$blob did not reach $(PLEXOS_NVIDIA_FWDIR)"; \
			echo "  nvidia.ko requests it by that exact path at runtime; without it"; \
			echo "  the module loads and the card does nothing."; \
			exit 1; }; \
	done; \
	test -s $(TARGET_DIR)/usr/share/licenses/nvidia/LICENSE || { \
		echo "plexos-nvidia: the NVIDIA licence did not reach the image."; \
		echo "  Clause 1.1(d) permits redistributing this firmware only if the"; \
		echo "  agreement is provided to each recipient. Shipping the blobs without"; \
		echo "  it is not a packaging slip, it is the condition being unmet."; \
		exit 1; }
	@for lib in $(PLEXOS_NVIDIA_LIBS); do \
		test -s $(TARGET_DIR)/usr/lib/$$lib.$(PLEXOS_NVIDIA_VERSION) || { \
			echo "plexos-nvidia: $$lib did not reach /usr/lib"; exit 1; }; \
		test -L $(TARGET_DIR)/usr/lib/$$lib || { \
			echo "plexos-nvidia: $$lib has no development symlink"; exit 1; }; \
	done; \
	test -x $(TARGET_DIR)/usr/bin/nvidia-smi || { \
		echo "plexos-nvidia: nvidia-smi did not reach the image, so there is no way to"; \
		echo "  ask this hardware anything from a shell."; exit 1; }
	@echo "plexos-nvidia: firmware, licence and userspace installed"
endef

PLEXOS_NVIDIA_POST_INSTALL_TARGET_HOOKS += PLEXOS_NVIDIA_CHECK_FIRMWARE


# Checked rather than trusted, because the failure this catches is silent. A module
# that reaches the target unsigned will be refused by the kernel at load and nothing
# in the build will have said a word about it.
define PLEXOS_NVIDIA_CHECK_SIGNED
	@found=0; unsigned=""; \
	for ko in $$(find $(TARGET_DIR)/usr/lib/modules -name 'nvidia*.ko*' 2>/dev/null); do \
		found=$$((found + 1)); \
		tail -c 28 "$$ko" | grep -q 'Module signature appended' || unsigned="$$unsigned $$(basename $$ko)"; \
	done; \
	if [ "$$found" -eq 0 ]; then \
		echo "plexos-nvidia: no nvidia modules reached the target"; \
		exit 1; \
	fi; \
	if [ -n "$$unsigned" ]; then \
		echo "plexos-nvidia: unsigned modules:$$unsigned"; \
		echo "  CONFIG_MODULE_SIG_FORCE=y refuses these at load, and the card will"; \
		echo "  do nothing on the machine with no message that says why. Sign them"; \
		echo "  with $(LINUX_DIR)/scripts/sign-file sha256 certs/signing_key.pem"; \
		echo "  certs/signing_key.x509, or check CONFIG_MODULE_SIG_ALL is still set."; \
		exit 1; \
	fi; \
	echo "plexos-nvidia: $$found modules, all signed"
endef

PLEXOS_NVIDIA_POST_INSTALL_TARGET_HOOKS += PLEXOS_NVIDIA_CHECK_SIGNED

$(eval $(generic-package))
