#!/usr/bin/env bash
# One-time host setup for building MediaLith images on this machine.
# Everything here needs root, which is why it is a script you run rather than
# something the assistant can do for itself.
#
# docs/DEVELOPMENT.md is the source of truth for host setup and explains why each
# of these is needed. This script is a convenience for a fresh machine; if the two
# ever disagree, the document is right.
set -euo pipefail

echo "==> Installing Buildroot host prerequisites"
# flex/bison are needed by the kernel build, not by Buildroot's own kconfig
# (which ships a pre-generated parser). qemu + ovmf are for testing the boot
# path under UEFI before touching the reference laptop.
#
# xfsprogs is the one that is not obvious: /var is XFS and Buildroot has no host
# xfsprogs package, so mkfs.xfs must come from the distribution. post-image.sh refuses
# without it -- correctly, and at the end of a full build.
sudo apt update
sudo apt install -y flex bison libncurses-dev qemu-system-x86 ovmf xfsprogs

echo
echo "==> Pointing /usr/bin/install at GNU install"
# Ubuntu resolute ships uutils coreutils, whose `install` is affected by
# https://github.com/uutils/coreutils/issues/12166 and which Buildroot refuses
# to build against. GNU install is already on disk as /usr/bin/gnuinstall.
# Reverse with: sudo update-alternatives --remove install /usr/bin/gnuinstall
sudo update-alternatives --install /usr/bin/install install /usr/bin/gnuinstall 100

echo
echo "==> Verifying"
install --version | head -1
for c in flex bison qemu-system-x86_64 mkfs.xfs; do
    printf '%-20s %s\n' "$c" "$(command -v "$c" || echo MISSING)"
done
ls /usr/share/OVMF/OVMF_CODE*.fd >/dev/null 2>&1 \
    && echo "OVMF                 present" \
    || echo "OVMF                 MISSING"

echo
echo "Done. Buildroot's own dependency check is the real verdict:"
echo "  cd /run/media/sgonera/TEMP/plexos-build/buildroot-upstream"
echo "  make BR2_EXTERNAL=$HOME/Documents/Projects/OS/buildroot \\"
echo "       O=/run/media/sgonera/TEMP/plexos-build/check dependencies"
