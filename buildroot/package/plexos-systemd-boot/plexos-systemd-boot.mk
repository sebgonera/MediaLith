################################################################################
#
# plexos-systemd-boot
#
# Builds only the UEFI bootloader and the UKI stub out of systemd's source tree,
# with none of systemd itself. See Config.in for why this exists rather than using
# Buildroot's BR2_PACKAGE_SYSTEMD_BOOT.
#
# The plexos- prefix is not decoration. Buildroot derives a package's enable symbol
# from its directory name, so a package called "systemd-boot" would declare
# BR2_PACKAGE_SYSTEMD_BOOT -- a symbol upstream already defines in
# package/systemd/Config.in. kconfig merges duplicate definitions instead of
# erroring, and upstream's systemd.mk separately assigns SYSTEMD_BOOT_EFI_ARCH.
# Both collisions happened to be harmless; neither was intended, and the prefix
# removes them rather than relying on that luck holding.
#
# The meson options and dependency list below are lifted from Buildroot's own
# package/systemd/systemd.mk, which is a configuration known to produce a working
# systemd-boot. Divergence from it is deliberate and commented where it occurs.
#
# NOT YET BUILT. Buildroot needs hours and a large amount of disk, which the
# development environment does not have. Expect the meson option list to need
# additions on the first real build: systemd's configure step insists on more
# being explicitly disabled than one would guess, and the failure is always a
# clear "unknown option" or "dependency not found" rather than anything subtle.
#
################################################################################

# Kept in step with Buildroot's systemd package so both fetch one tarball.
PLEXOS_SYSTEMD_BOOT_VERSION = 258.7
PLEXOS_SYSTEMD_BOOT_SITE = $(call github,systemd,systemd,v$(PLEXOS_SYSTEMD_BOOT_VERSION))

# Both of these must be set, and neither is cosmetic. Buildroot names a downloaded
# tarball after the *package* -- $(PKG)_BASENAME_RAW.tar.gz -- and stores it in a
# directory of the same name. Left to the defaults this package would fetch
# "plexos-systemd-boot-258.7.tar.gz" into dl/plexos-systemd-boot/, and the download
# would fail outright: the hash file names systemd-258.7.tar.gz, so there would be no
# hash entry for the file actually retrieved.
#
# Naming them explicitly makes the tarball and its directory identical to the ones
# Buildroot's own systemd package uses, so the two genuinely share one download --
# which is what the comment above always claimed and, until this was set, was not
# true of either this package or its differently-named predecessor.
PLEXOS_SYSTEMD_BOOT_SOURCE = systemd-$(PLEXOS_SYSTEMD_BOOT_VERSION).tar.gz
PLEXOS_SYSTEMD_BOOT_DL_SUBDIR = systemd
PLEXOS_SYSTEMD_BOOT_LICENSE = LGPL-2.1+
PLEXOS_SYSTEMD_BOOT_LICENSE_FILES = LICENSE.LGPL2.1

# Taken from Buildroot's own systemd package rather than derived from what the
# bootloader appears to need, and the difference matters: meson configures the whole
# of systemd even when only two targets will be built, so its configure-time
# requirements apply in full.
#
#   host-gperf            meson.build:620  find_program('gperf'), not optional
#   host-python-jinja2    meson.build:1687 find_installation(..., required : true,
#                                          modules : ['jinja2'])
#   host-python-pyelftools           elf2efi.py, which converts each ELF to a PE
#
# The first two were missing until the first real build, which failed at configure
# with "Program 'gperf' not found or not executable" -- exactly the clear,
# early, name-the-missing-thing failure the note above predicted.
# libcap and libxcrypt are target libraries, and they are here only to satisfy
# configure. meson.build:684 hard-errors if crypt.h or sys/capability.h is absent,
# with no option guarding it, even though neither is reachable from the two EFI
# binaries this package builds. So they end up in the image to make the bootloader
# compile, which is in tension with the rule that nothing enters the image unless
# Plex needs it. Both are small, and the honest fix is to stop configuring the whole
# systemd tree rather than to argue about the dependency -- see the note at the end
# of this file.
PLEXOS_SYSTEMD_BOOT_DEPENDENCIES = \
	host-pkgconf \
	gnu-efi \
	host-gperf \
	host-python-jinja2 \
	host-python-pyelftools \
	libcap \
	libxcrypt

# The bootloader is a freestanding UEFI binary. Nothing belongs in the target
# filesystem or in staging: it is placed on the ESP by post-image.sh, and never
# runs from a mounted filesystem.
PLEXOS_SYSTEMD_BOOT_INSTALL_STAGING = NO
PLEXOS_SYSTEMD_BOOT_INSTALL_TARGET = NO
PLEXOS_SYSTEMD_BOOT_INSTALL_IMAGES = YES

PLEXOS_SYSTEMD_BOOT_EFI_ARCH = $(call qstrip,$(BR2_PACKAGE_PLEXOS_SYSTEMD_BOOT_EFI_ARCH))
PLEXOS_SYSTEMD_BOOT_EFI_NAME = systemd-boot$(PLEXOS_SYSTEMD_BOOT_EFI_ARCH).efi

# The UKI stub, and not an optional extra. A Unified Kernel Image *is* this stub
# with .osrel/.cmdline/.linux/.initrd sections appended (ADR-0004), so without it
# post-image.sh has nothing to build an image around. Upstream Buildroot's systemd
# package never installs it, which is a second reason this package exists.
#
# Verified against systemd v258.7 src/boot/meson.build rather than recalled: the
# executables are named "linux$(arch)" with name_suffix "elf.stub", and elf2efi.py
# converts each to "<name>.efi.stub". Hence linuxx64.efi.stub on x86-64.
PLEXOS_SYSTEMD_BOOT_STUB_NAME = linux$(PLEXOS_SYSTEMD_BOOT_EFI_ARCH).efi.stub

PLEXOS_SYSTEMD_BOOT_NINJA_TARGETS = \
	src/boot/$(PLEXOS_SYSTEMD_BOOT_EFI_NAME) \
	src/boot/$(PLEXOS_SYSTEMD_BOOT_STUB_NAME)

# Everything off except the bootloader. systemd's meson still configures the whole
# tree, so each subsystem has to be turned off by name; only the bootloader target
# is actually built (see PLEXOS_SYSTEMD_BOOT_BUILD_CMDS).
PLEXOS_SYSTEMD_BOOT_CONF_OPTS = \
	-Dbootloader=enabled \
	-Defi=true \
	-Dmode=release \
	-Dlink-boot-shared=true \
	-Dman=disabled \
	-Dtests=false \
	-Dfuzz-tests=false \
	-Dinstall-tests=false \
	-Dvcs-tag=false \
	-Dukify=disabled \
	-Ddbus=disabled \
	-Dglib=disabled \
	-Dbpf-framework=disabled \
	-Dvmlinux-h=disabled \
	-Dtpm2=disabled \
	-Dlibfido2=disabled \
	-Dpasswdqc=disabled \
	-Dlibarchive=disabled \
	-Dxenctrl=disabled \
	-Dsysupdated=disabled \
	-Dlog-message-verification=disabled \
	-Dlibmount=disabled \
	-Didn=false \
	-Dima=false \
	-Dipe=false \
	-Dldconfig=false \
	-Dnss-systemd=false \
	-Dtmpfiles=false \
	-Dcreate-log-dirs=false \
	-Dfirst-boot-full-preset=false \
	-Dsysvinit-path= \
	-Dsysvrcnd-path=

# Build only the bootloader target. The meson infrastructure would otherwise run
# ninja over the whole of systemd -- minutes of compilation producing a userspace
# this package then throws away. Overriding BUILD_CMDS rather than appending to
# NINJA_OPTS because the infrastructure places NINJA_OPTS ahead of -C, and a ninja
# target argument there is not reliably parsed.
define PLEXOS_SYSTEMD_BOOT_BUILD_CMDS
	$(TARGET_MAKE_ENV) $(PLEXOS_SYSTEMD_BOOT_NINJA_ENV) \
		$(NINJA) $(NINJA_OPTS) -C $(@D)/buildroot-build $(PLEXOS_SYSTEMD_BOOT_NINJA_TARGETS)
endef

# Flat in BINARIES_DIR rather than pre-arranged into an efi-part/ tree. Where the
# bootloader goes on the ESP, and how boot entries are named, is MediaLith policy
# expressed in post-image.sh and the installer -- not something a package that
# compiles a binary should be deciding.
define PLEXOS_SYSTEMD_BOOT_INSTALL_IMAGES_CMDS
	$(INSTALL) -D -m 0644 \
		$(@D)/buildroot-build/src/boot/$(PLEXOS_SYSTEMD_BOOT_EFI_NAME) \
		$(BINARIES_DIR)/$(PLEXOS_SYSTEMD_BOOT_EFI_NAME)
	$(INSTALL) -D -m 0644 \
		$(@D)/buildroot-build/src/boot/$(PLEXOS_SYSTEMD_BOOT_STUB_NAME) \
		$(BINARIES_DIR)/$(PLEXOS_SYSTEMD_BOOT_STUB_NAME)
endef

$(eval $(meson-package))

# ---------------------------------------------------------------------------
# Known cost, not yet paid down
# ---------------------------------------------------------------------------
# This package configures the whole of systemd in order to build two freestanding
# EFI binaries. That is why its dependency list keeps growing with things the
# bootloader cannot possibly use: gperf, jinja2, libcap, libxcrypt. Each was added
# because meson refused to configure without it, and the last two are target
# libraries that consequently ship in the image.
#
# The image-size rule in buildroot/README.md says nothing enters the base image
# unless Plex needs it to run, and these do not. Two candidates for fixing it:
# build this as a host package, since the EFI binaries are freestanding and link
# nothing from the target sysroot; or carry a patch that lets systemd's meson
# configure the bootloader alone. Neither is worth doing before an image has booted.
