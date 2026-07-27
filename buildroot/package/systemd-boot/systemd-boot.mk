################################################################################
#
# systemd-boot (standalone)
#
# Builds only the UEFI bootloader out of systemd's source tree, with none of
# systemd itself. See package/systemd-boot/Config.in for why this exists rather
# than using Buildroot's BR2_PACKAGE_SYSTEMD_BOOT.
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
SYSTEMD_BOOT_VERSION = 258.7
SYSTEMD_BOOT_SITE = $(call github,systemd,systemd,v$(SYSTEMD_BOOT_VERSION))
SYSTEMD_BOOT_LICENSE = LGPL-2.1+
SYSTEMD_BOOT_LICENSE_FILES = LICENSE.LGPL2.1

SYSTEMD_BOOT_DEPENDENCIES = host-pkgconf gnu-efi host-python-pyelftools

# The bootloader is a freestanding UEFI binary. Nothing belongs in the target
# filesystem or in staging: it is placed on the ESP by post-image.sh, and never
# runs from a mounted filesystem.
SYSTEMD_BOOT_INSTALL_STAGING = NO
SYSTEMD_BOOT_INSTALL_TARGET = NO
SYSTEMD_BOOT_INSTALL_IMAGES = YES

SYSTEMD_BOOT_EFI_ARCH = $(call qstrip,$(BR2_PACKAGE_SYSTEMD_BOOT_EFI_ARCH))
SYSTEMD_BOOT_EFI_NAME = systemd-boot$(SYSTEMD_BOOT_EFI_ARCH).efi
SYSTEMD_BOOT_NINJA_TARGET = src/boot/$(SYSTEMD_BOOT_EFI_NAME)

# Everything off except the bootloader. systemd's meson still configures the whole
# tree, so each subsystem has to be turned off by name; only the bootloader target
# is actually built (see SYSTEMD_BOOT_BUILD_CMDS).
SYSTEMD_BOOT_CONF_OPTS = \
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
define SYSTEMD_BOOT_BUILD_CMDS
	$(TARGET_MAKE_ENV) $(SYSTEMD_BOOT_NINJA_ENV) \
		$(NINJA) $(NINJA_OPTS) -C $(@D)/buildroot-build $(SYSTEMD_BOOT_NINJA_TARGET)
endef

# Flat in BINARIES_DIR rather than pre-arranged into an efi-part/ tree. Where the
# bootloader goes on the ESP, and how boot entries are named, is PlexOS policy
# expressed in post-image.sh and the installer -- not something a package that
# compiles a binary should be deciding.
define SYSTEMD_BOOT_INSTALL_IMAGES_CMDS
	$(INSTALL) -D -m 0644 \
		$(@D)/buildroot-build/$(SYSTEMD_BOOT_NINJA_TARGET) \
		$(BINARIES_DIR)/$(SYSTEMD_BOOT_EFI_NAME)
endef

$(eval $(meson-package))
