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
# What has NOT been done is Buildroot driving that build. This file is the part
# that is unproven, and the notice stays until a build has run it.
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
# GSP firmware is requested at runtime from nvidia/$(VERSION)/gsp_ga10x.bin and is
# NOT in this tarball -- it ships in NVIDIA's .run installer. Only two families are
# ever asked for, gsp_tu10x for Turing and gsp_ga10x for Ampere and everything after
# it, Blackwell included. That is ADR-0015 step 4 and deliberately not here.
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
