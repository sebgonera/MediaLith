#!/usr/bin/env bash
#
# Assemble a bootable PlexOS disk image.
#
# All six stages run, and the image they produce boots: systemd-boot loads the UKI,
# dm-verity opens, /usr mounts read-only from it, /var and the /etc overlay come up,
# and switch_root hands over to the service manager. Verified under QEMU from inside
# the running system, not inferred from the build succeeding.
#
# post-image-test.sh exercises the stages individually against real tools in seconds,
# which is the faster loop when changing anything here.
#
# Not yet exercised by any of this: signing. Images are unsigned, so Secure Boot must
# be off. See ADR-0004, which leaves key handling deliberately undecided.
#
# The ordering below is forced by ADR-0004 and this script is where it is enforced:
#
#     /usr image  ->  verity tree  ->  root hash  ->  UKI command line  ->  signature
#
# Every arrow is a real dependency. The root hash cannot be known before the image
# exists, the command line cannot be written before the root hash is known, and the
# signature must cover the command line. Reordering any pair silently produces an
# image whose signature does not cover what it boots, which is indistinguishable from
# a working image until dm-verity refuses to open on a user's machine.
#
# Buildroot passes BINARIES_DIR as $1, and exports PATH (with $HOST_DIR/bin and
# $HOST_DIR/sbin ahead of the system ones), O, BUILD_DIR, and BR2_EXTERNAL_PLEXOS_PATH.
# It does not export HOST_DIR or TARGET_DIR, so those are derived here.

set -euo pipefail

BINARIES_DIR="${1:?post-image.sh: BINARIES_DIR not passed; Buildroot supplies it as \$1}"
HOST_DIR="$(cd "${BINARIES_DIR}/../host" && pwd)"
TARGET_DIR="$(cd "${BINARIES_DIR}/../target" && pwd)"
BOARD_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${BOARD_DIR}/../../../.." && pwd)"

WORK="${BINARIES_DIR}/plexos-work"
IMAGE="${BINARIES_DIR}/plexos.img"

# --------------------------------------------------------------------------
# Knobs
# --------------------------------------------------------------------------
# Total image size. /var takes whatever is left after the fixed partitions, which
# come to ~2.6 GiB (ADR-0003), so this is the smallest round number that leaves a
# usable /var. A real installation grows /var to the disk; this is a test image.
PLEXOS_IMAGE_SIZE="${PLEXOS_IMAGE_SIZE:-8G}"

# The version, and with it the anti-rollback sequence.
#
# The default carries a UTC build stamp because a version without one cannot be updated
# *to*: systemd-boot orders entries by this string, so two builds both called 0.1.0 sort
# equal and the bootloader keeps choosing the entry already there -- an update that writes
# a slot, installs an entry, reboots, and changes nothing. The stamp is also the manifest's
# `sequence` (ADR-0006), which is what stops an old release being replayed at a machine.
#
# Taken from SOURCE_DATE_EPOCH when that is set, so a deliberately reproducible build stays
# reproducible: the version reaches os-release, which is inside /usr, which is covered by
# the verity root hash.
if [ -z "${PLEXOS_VERSION:-}" ]; then
    if [ "${SOURCE_DATE_EPOCH:-0}" -ne 0 ] 2>/dev/null; then
        PLEXOS_VERSION="0.1.0.$(date -u -d "@${SOURCE_DATE_EPOCH}" +%Y%m%d%H%M)"
    else
        PLEXOS_VERSION="0.1.0.$(date -u +%Y%m%d%H%M)"
    fi
fi

# Reproducibility. mkfs.erofs stamps a build time and veritysetup generates a random
# salt unless told otherwise; either one makes two builds of identical inputs produce
# different root hashes, which destroys the ability to verify that a released image
# corresponds to a given commit.
#
# The salt is public — it lives in the verity superblock — and exists to stop one
# precomputed table serving every image. A fixed value is therefore not a weakness,
# but it must be a *deliberate* fixed value rather than an accidental one.
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}"
PLEXOS_VERITY_SALT="${PLEXOS_VERITY_SALT:-706c65786f732d76657269747900000000000000000000000000000000000000}"

# mkfs.erofs also writes a randomly generated filesystem UUID into the superblock,
# and -T does not cover it. Two builds of byte-identical inputs otherwise differ in
# exactly 20 bytes: the 16-byte UUID at superblock offset 0x30, and the 4-byte
# checksum computed over it. That is enough to change the verity root hash, and with
# it the signed command line.
#
# Pinning it is safe because nothing ever identifies the /usr image by UUID:
# plexos-init mounts /dev/mapper/plexos-usr, having found the partition by label
# (ADR-0003). Both slots therefore carrying the same filesystem UUID is intended.
PLEXOS_EROFS_UUID="${PLEXOS_EROFS_UUID:-706c6578-6f73-0000-0000-000000000001}"

# Secure Boot signing. Unset by default: development images are self-signed at most,
# and ADR-0004 leaves key handling explicitly undecided. When both are set the UKI is
# signed; when neither is, it is not, and the script says so rather than silently
# producing an unsigned artifact that looks signed.
PLEXOS_SB_KEY="${PLEXOS_SB_KEY:-}"
PLEXOS_SB_CERT="${PLEXOS_SB_CERT:-}"

# The Buildroot source tree, needed for support/scripts/mkusers (see stage 0). Buildroot
# exports BUILD_DIR, CONFIG_DIR, O, BR2_DL_DIR and PARALLEL_JOBS to a post-image script
# (package/Makefile.in, EXTRA_ENV) but not TOPDIR, so it is derived below. Set this only
# when that derivation fails; it names the directory holding support/scripts/mkusers.
PLEXOS_BUILDROOT_DIR="${PLEXOS_BUILDROOT_DIR:-}"

msg()  { printf '>>> plexos: %s\n' "$*"; }
die()  { printf >&2 '\nplexos post-image: %s\n' "$1"; [ $# -gt 1 ] && printf >&2 '  remedy: %s\n' "$2"; exit 1; }

# --------------------------------------------------------------------------
# Preflight
# --------------------------------------------------------------------------
# Checked up front and all at once. A missing tool discovered at step six wastes the
# five steps before it, and on this project a build is measured in hours.
preflight() {
    local missing=0

    # From Buildroot's host tree, so they are pinned with the rest of the build.
    for tool in \
        "${HOST_DIR}/bin/mkfs.erofs:BR2_TARGET_ROOTFS_EROFS (pulls host-erofs-utils)" \
        "${HOST_DIR}/sbin/veritysetup:BR2_PACKAGE_HOST_CRYPTSETUP" \
        "${HOST_DIR}/sbin/sgdisk:BR2_PACKAGE_HOST_GPTFDISK" \
        "${HOST_DIR}/sbin/mkfs.vfat:BR2_PACKAGE_HOST_DOSFSTOOLS" \
        "${HOST_DIR}/bin/mcopy:BR2_PACKAGE_HOST_MTOOLS"
    do
        local path="${tool%%:*}" opt="${tool#*:}"
        if [ ! -x "${path}" ]; then
            printf >&2 'missing host tool: %s\n  provided by: %s\n' "${path}" "${opt}"
            missing=1
        fi
    done

    # From the build host. mkfs.xfs is the one filesystem tool Buildroot has no host
    # package for, so it is a genuine host requirement rather than an oversight.
    for tool in objcopy objdump readelf cpio mkfs.xfs; do
        command -v "${tool}" >/dev/null 2>&1 || {
            printf >&2 'missing host tool: %s\n' "${tool}"
            missing=1
        }
    done

    [ "${missing}" -eq 0 ] || die \
        "the tools above are required to assemble an image" \
        "install the host packages listed, or enable the Buildroot options named (docs/DEVELOPMENT.md)"

    [ -f "${BINARIES_DIR}/bzImage" ] || die \
        "no kernel at ${BINARIES_DIR}/bzImage" \
        "BR2_LINUX_KERNEL_BZIMAGE must be set; check the kernel actually built"

    [ -f "${BINARIES_DIR}/linuxx64.efi.stub" ] || die \
        "no UKI stub at ${BINARIES_DIR}/linuxx64.efi.stub" \
        "package/plexos-systemd-boot must install it; upstream Buildroot's systemd package does not"

    [ -f "${BINARIES_DIR}/systemd-bootx64.efi" ] || die \
        "no bootloader at ${BINARIES_DIR}/systemd-bootx64.efi" \
        "set BR2_PACKAGE_PLEXOS_SYSTEMD_BOOT=y"

    [ -d "${TARGET_DIR}/usr" ] || die \
        "no ${TARGET_DIR}/usr to build an image from" \
        "the target filesystem did not build; check the Buildroot log"
}

# --------------------------------------------------------------------------
# 0. The users table, which Buildroot applies too late for this image to see it
# --------------------------------------------------------------------------
# Buildroot does not apply BR2_ROOTFS_USERS_TABLES to TARGET_DIR. It rsyncs TARGET_DIR
# into a per-filesystem copy and runs mkusers against *that* (fs/common.mk: the rsync at
# the top of the image rule, then `mkusers $(ROOTFS_FULL_USERS_TABLE) $(TARGET_DIR)`
# where TARGET_DIR has been retargeted to the copy), then deletes the copy. So
# TARGET_DIR/etc/passwd never gains the accounts at all, and anything here that reads
# TARGET_DIR/etc reads the tree from before the users table existed.
#
# That is not a detail. The factory /etc staged in stage 1 becomes the lower layer of
# the appliance's /etc overlay, so an account missing from it is missing from the running
# system -- while being present in Buildroot's own rootfs.erofs, which is what makes the
# mistake so hard to see. privilege::drop_to takes numbers and works regardless, so uid
# 900 runs Plex perfectly well until something calls getpwuid and finds no name, no home
# and no shell.
#
# The fix is to run the same script Buildroot runs, against TARGET_DIR, before staging.
# Re-deriving the passwd/group/shadow format here instead would be a second source of
# truth for a file format that already has one.
resolve_buildroot_dir() {
    local candidate

    if [ -n "${PLEXOS_BUILDROOT_DIR}" ]; then
        printf '%s\n' "${PLEXOS_BUILDROOT_DIR}"
        return 0
    fi

    # Out-of-tree build, which is how docs/DEVELOPMENT.md builds this: Buildroot writes
    # a wrapper $(O)/Makefile whose MAKEARGS line names the tree it came from.
    if [ -n "${O:-}" ] && [ -f "${O}/Makefile" ]; then
        candidate="$(sed -n 's/^MAKEARGS := -C //p' "${O}/Makefile" | head -n 1)"
        if [ -n "${candidate}" ] && [ -x "${candidate}/support/scripts/mkusers" ]; then
            printf '%s\n' "${candidate}"
            return 0
        fi
    fi

    # In-tree build: O defaults to $(TOPDIR)/output and no wrapper Makefile is written.
    candidate="$(dirname "${O:-${BINARIES_DIR}/..}")"
    if [ -x "${candidate}/support/scripts/mkusers" ]; then
        printf '%s\n' "${candidate}"
        return 0
    fi

    return 1
}

apply_users_table() {
    local topdir table

    topdir="$(resolve_buildroot_dir)" || die \
        "cannot locate the Buildroot source tree, so the users table cannot be applied" \
        "set PLEXOS_BUILDROOT_DIR to the directory that holds support/scripts/mkusers"

    # The *merged* table, not board/users.table. Buildroot concatenates the users
    # declared by packages with the ones from BR2_ROOTFS_USERS_TABLES into this file
    # (fs/common.mk, ROOTFS_FULL_USERS_TABLE). Reading our own table instead would
    # silently drop every account a package asked for, which is the same class of bug
    # as the one this stage exists to fix.
    table="${BUILD_DIR:-}/buildroot-fs/full_users_table.txt"
    [ -f "${table}" ] || die \
        "no merged users table at ${table}" \
        "BUILD_DIR must point at the Buildroot build directory; it is exported to post-image scripts, so an empty one means this ran outside a Buildroot build"

    # mkusers reads BR2_TARGET_GENERIC_PASSWD_METHOD straight out of the config file.
    [ -n "${BR2_CONFIG:-}" ] && [ -f "${BR2_CONFIG}" ] || die \
        "BR2_CONFIG is unset or does not name a file, and mkusers reads the password method from it" \
        "run this from a Buildroot build, which exports BR2_CONFIG"

    msg "applying the users table to the target tree"

    # stdout is chown commands for Buildroot's fakeroot script, not diagnostics.
    # Discarding them is right here: the /usr image is built --all-root, and the home
    # directory mkusers creates lives under /var, which is not in the image and is
    # created with its ownership at runtime.
    #
    # BR2_CONFIG is passed in the invocation environment rather than relied on being
    # exported. Buildroot does export it, but mkusers is a separate process, so a
    # caller that merely set it as a shell variable would leave mkusers reading the
    # password method out of an empty filename -- which fails as `sed: can't read :`,
    # naming neither the variable nor the config file.
    BR2_CONFIG="${BR2_CONFIG}" \
        "${topdir}/support/scripts/mkusers" "${table}" "${TARGET_DIR}" >/dev/null || die \
        "mkusers failed against ${TARGET_DIR}; it has printed the reason above" \
        "the usual cause is an account in ${table} disagreeing with one already in ${TARGET_DIR}/etc/passwd, which a clean build clears"

    msg "  $(awk -F: 'END { print NR }' "${TARGET_DIR}/etc/passwd") accounts in the target's /etc/passwd"
}

# Every account this board declares must be in the tree that reaches the appliance, with
# the uid it was declared with. Checked against board/users.table rather than against
# whatever apply_users_table did, so that the check still holds if the mechanism above is
# ever replaced -- and checked on the staged factory tree, which is what actually ships.
check_factory_accounts() {
    local factory="${1}" username uid found

    while read -r username uid _; do
        case "${username}" in ''|'#'*|'-') continue;; esac
        # A negative uid asks mkusers to allocate one, so there is nothing to compare
        # against. board/users.table forbids it and says why; this just avoids
        # reporting a mismatch that would be meaningless.
        case "${uid}" in -*) continue;; esac

        found="$(awk -F: -v u="${username}" '$1 == u { print $3 }' "${factory}/passwd")"

        [ -n "${found}" ] || die \
            "the '${username}' account is not in the factory /etc that goes into the image" \
            "apply_users_table must run before the factory /etc is staged; Buildroot's own mkusers pass never touches TARGET_DIR"

        [ "${found}" = "${uid}" ] || die \
            "the '${username}' account has uid ${found} in the factory /etc, but users.table declares ${uid}" \
            "an earlier build left a conflicting entry in ${TARGET_DIR}/etc/passwd; remove it, or run a clean build"
    done < "${BOARD_DIR}/users.table"
}

# --------------------------------------------------------------------------
# 0b. The version this system will report
# --------------------------------------------------------------------------
# Before the /usr image, and that ordering is the whole point. This was first written
# inside build_uki, which is stage 4 -- so the UKI carried the right version and
# /usr/lib/os-release, baked in stage 1, still said "Buildroot 2026.02.3". The appliance
# would then report Buildroot's release as its own, and plexos-update compares that
# string against the bundle's: 2026 sorts above 0, so every update would have been
# refused as older, blaming the publisher for a mistake made here.
#
# Buildroot's own os-release is replaced rather than extended. Two answers to "what
# version is this" is one too many, and the wrong one was winning.
#
# SORT_KEY is what systemd-boot groups entries by before it compares versions; entries
# with one and entries without sort by different rules, so every entry gets it.
stage_os_release() {
    msg "stamping the version into os-release"
    printf 'NAME="PlexOS"\nID=plexos\nVERSION_ID=%s\nPRETTY_NAME="PlexOS %s"\nSORT_KEY=plexos\n' \
        "${PLEXOS_VERSION}" "${PLEXOS_VERSION}" > "${WORK}/os-release"
    install -D -m 0644 "${WORK}/os-release" "${TARGET_DIR}/usr/lib/os-release"
    msg "  VERSION_ID=${PLEXOS_VERSION}"
}

# --------------------------------------------------------------------------
# 1. The /usr image
# --------------------------------------------------------------------------
# Only /usr, not the whole target tree. /usr *is* the unit of update (ADR-0001), and
# the root is a tmpfs assembled at boot, so anything outside /usr in the target
# directory is either recreated by plexos-init or deliberately discarded.
build_usr_image() {
    # The /etc overlay takes its lower layer from inside the read-only image
    # (paths::ETC_FACTORY), and its upper layer from /var. Buildroot leaves its
    # configuration in /etc, which is not part of /usr and therefore is not in the
    # image at all -- so without this the overlay mount fails with ENOENT on the
    # lower directory, fifteen steps into an otherwise working boot.
    #
    # Staged into TARGET_DIR rather than a copy of it, so mkfs.erofs still has a
    # single source tree. --all-root below makes the ownership right regardless of
    # who ran the build.
    local factory="${TARGET_DIR}/usr/share/factory/etc"
    msg "staging factory /etc into the image"
    rm -rf "${factory}"
    mkdir -p "${factory}"
    cp -a "${TARGET_DIR}/etc/." "${factory}/"
    msg "  $(find "${factory}" -mindepth 1 -maxdepth 1 | wc -l) entries"
    check_factory_accounts "${factory}"

    msg "building /usr erofs image"
    "${HOST_DIR}/bin/mkfs.erofs" \
        --quiet \
        -zlz4hc \
        --all-root \
        -T "${SOURCE_DATE_EPOCH}" \
        -U "${PLEXOS_EROFS_UUID}" \
        "${WORK}/usr.erofs" \
        "${TARGET_DIR}/usr"

    local size limit
    size=$(stat -c %s "${WORK}/usr.erofs")
    limit=$((1024 * 1024 * 1024))   # usr_a, from LAYOUT_X86_64
    msg "  /usr image is $((size / 1024 / 1024)) MiB of $((limit / 1024 / 1024)) MiB"
    [ "${size}" -le "${limit}" ] || die \
        "/usr image is larger than its partition ($(( size / 1024 / 1024 )) MiB > $(( limit / 1024 / 1024 )) MiB)" \
        "remove packages from the defconfig; the partition size is frozen by ADR-0003 and cannot be raised in the field"
}

# --------------------------------------------------------------------------
# 2. The verity tree, and the root hash everything downstream depends on
# --------------------------------------------------------------------------
build_verity() {
    msg "computing dm-verity tree"
    local out
    out=$("${HOST_DIR}/sbin/veritysetup" format \
              --salt="${PLEXOS_VERITY_SALT}" \
              "${WORK}/usr.erofs" "${WORK}/usr.hash")

    ROOT_HASH=$(printf '%s\n' "${out}" | awk '/^Root hash:/ { print $NF }')
    [ -n "${ROOT_HASH}" ] || die \
        "could not parse a root hash out of veritysetup" \
        "run veritysetup format by hand and check its output format has not changed"
    # 64 hex characters, since the tree is SHA-256. A truncated hash would still look
    # plausible on the kernel command line and fail only at boot.
    [ "${#ROOT_HASH}" -eq 64 ] || die \
        "root hash is ${#ROOT_HASH} characters, expected 64: ${ROOT_HASH}" \
        "check the verity hash algorithm; plexos-init assumes SHA-256"

    local size limit
    size=$(stat -c %s "${WORK}/usr.hash")
    limit=$((32 * 1024 * 1024))     # usr_a_hash, from LAYOUT_X86_64
    [ "${size}" -le "${limit}" ] || die \
        "verity tree does not fit its partition ($(( size / 1024 / 1024 )) MiB > 32 MiB)" \
        "the hash partition size is frozen by ADR-0003; a smaller /usr is the only lever"

    msg "  root hash ${ROOT_HASH}"
}

# --------------------------------------------------------------------------
# 3. The initrd
# --------------------------------------------------------------------------
# One statically linked binary as /init, and nothing else. No module loader, no udev,
# nothing that can go stale relative to the kernel it ships inside (ARCHITECTURE.md
# §3). The static link is what makes that possible: a dynamic plexos-init would drag
# the loader and libc into the initrd, and they would have to be kept in step with a
# /usr this early code cannot yet read.
# GuC and HuC, into the initrd rather than into /usr.
#
# i915 is built into the kernel (CONFIG_DRM_I915=y) and fetches this firmware while it
# probes, during do_initcalls. /usr does not exist yet at that moment and will not for
# another second or so, so firmware living there is firmware i915 never sees: it
# continues without GuC and HuC, silently, and the only symptom is transcodes that come
# out worse than they should at a given bitrate.
#
# The initramfs is unpacked by rootfs_initcall, which runs *before* the device_initcall
# that probes i915, so this is the earliest place the files can be and still be found.
#
# The alternative is CONFIG_EXTRA_FIRMWARE, which compiles them into the kernel. It
# works, and it needs CONFIG_EXTRA_FIRMWARE_DIR to be an absolute path into Buildroot's
# target directory — a path that differs per build tree and cannot be written into a
# kconfig fragment that is checked in. This keeps image assembly in the script that
# does image assembly.
#
# Every GuC and HuC blob, not a list of the ones the developer's own laptop wants.
#
# This was two filenames — the Kaby Lake pair Whiskey Lake-U asks for — and it worked
# perfectly on the machine it was written on. Moving the image to an Alder Lake laptop
# produced hardware transcoding that ran but at lower quality than the chip can manage,
# because i915 asked for adlp_guc_70.bin, found nothing, and continued without it. The
# blob was in /usr the whole time; it was the initramfs that lacked it.
#
# The failure is quiet by construction. i915 does not fail to probe over missing GuC
# firmware, it carries on, and the only place that says so is a debugfs file nobody reads
# — which is why the appliance's own GPU report now parses that file's "MISSING" and
# "ERROR" rather than shrugging.
#
# Roughly 25 MiB, which lands in both UKIs and so twice in every update bundle. That is
# the price of an appliance image that works on the hardware it is put on rather than on
# the hardware it was built on, and after three machines in one day the argument is not
# close.
install_gpu_firmware() {
    local from="${TARGET_DIR}/usr/lib/firmware"
    local total=0
    local count=0

    # Globbed rather than listed. A list has to be revised for every generation and is
    # only ever revised after somebody notices, which is the whole story above.
    for blob in "${from}"/i915/*guc*.bin "${from}"/i915/*huc*.bin; do
        [ -e "${blob}" ] || continue
        install -D -m 0444 "${blob}" "${WORK}/initrd/lib/firmware/i915/$(basename "${blob}")"
        total=$(( total + $(stat -c %s "${blob}") ))
        count=$(( count + 1 ))
    done

    [ "${count}" -gt 0 ] || die \
        "linux-firmware provided no i915 GuC or HuC blobs at all" \
        "check BR2_PACKAGE_LINUX_FIRMWARE_I915 is still set; without these, hardware transcoding runs at reduced quality on every Intel machine and says so only in debugfs"

    msg "  GuC/HuC firmware in initrd: ${count} blobs, $(( total / 1024 )) KiB"

    install_xe_firmware
    install_wifi_firmware
}

# The other Intel driver, which this image builds and has never fed.
#
# CONFIG_DRM_XE=y, so `xe` binds to the parts it claims — Lunar Lake, Battlemage, Panther
# Lake — and until now the image carried firmware for `i915` and nothing else. The claim
# that current Arc hardware works here was therefore softer than it read: the card binds,
# and then asks for a file that is not in the initramfs.
#
# `xe` asks by directory. `xe_uc_fw.c` builds the name as `xe/<plat>_guc_<major>.bin` and
# `xe/<plat>_huc.bin`, so the subdirectory is part of the request and a flat copy is a copy
# the driver never finds. Same shape as the i915 blobs above, which is why those are placed
# under `i915/` rather than beside them.
#
# Everything in the directory, not a glob of guc and huc. It holds eight files and four
# mebibytes, so there is nothing to gain by choosing among them — and two of them are
# neither GuC nor HuC: `lnl_gsc_1.bin`, which is the security controller HuC
# authentication goes through on these parts, and a fan-control blob. A pattern written
# for the two familiar names would drop both, which is precisely the mistake the
# generation-by-generation firmware list already made once.
#
# Not fatal when the directory is absent, unlike i915. A build without
# BR2_PACKAGE_LINUX_FIRMWARE_XE is a legitimate build for a machine with no such GPU, and
# every machine this has ever run on is in that category. post-image-test.sh asserts the
# blobs are present instead, so the option silently disappearing from the defconfig — the
# way four options once did — is a failed test rather than an appliance that transcodes
# badly and says so only in debugfs.
install_xe_firmware() {
    local from="${TARGET_DIR}/usr/lib/firmware/xe"
    local total=0
    local count=0
    local blob

    for blob in "${from}"/*; do
        [ -f "${blob}" ] || continue
        install -D -m 0444 "${blob}" "${WORK}/initrd/lib/firmware/xe/$(basename "${blob}")"
        total=$(( total + $(stat -Lc %s "${blob}") ))
        count=$(( count + 1 ))
    done

    if [ "${count}" -gt 0 ]; then
        msg "  xe firmware in initrd: ${count} blobs, $(( total / 1024 )) KiB"
    else
        msg "  no xe firmware found -- Arc and Xe2 parts will bind and run without it"
    fi
}

# iwlwifi, for exactly the same reason and with exactly the same failure.
#
# CONFIG_IWLWIFI=y, so the driver probes during do_initcalls and asks for its firmware a
# second before /usr is mounted. Blobs in /usr are blobs it never sees -- and unlike i915,
# which carries on in a degraded mode, iwlwifi with no firmware registers no netdev at
# all. The symptom is that `wlan0` does not exist on a machine whose wireless card is
# fitted, working and named correctly by lspci, which reads as a card the kernel does not
# support rather than as a missing file.
#
# The blobs in linux-firmware's root are symlinks into intel/iwlwifi, so both the copy and
# the size have to follow them: `install` does by default and `stat` does not.
#
# Only the newest API revision of each variant is carried, and that is what makes covering
# more than one machine affordable. iwlwifi asks for one revision and counts down to its
# minimum -- and for every family this image enables the kernel's IWL_*_UCODE_API_MIN is
# equal to its _MAX (6.19.14: 46 for the 9000s, 77 for Qu, QuZ and cc-a0, 89 for the AX210
# family), so exactly one file per variant is ever opened and every other revision is a
# megabyte and a half riding in both UKIs and in every update bundle. linux-firmware ships
# seven revisions of each Qu part and thirteen of ty-a0-gf-a0. Shipping all of them came to
# 70 MiB and covered neither AX210 nor AX211; shipping the newest comes to 22 MiB and
# covers both.
#
# "Newest available" is the right file only for as long as linux-firmware does not run
# ahead of the kernel. If it ever does -- a revision shipped that this kernel will not ask
# for -- the card goes back to registering no netdev at all, which is the failure above
# wearing the disguise of a firmware directory that is visibly full. post-image-test.sh
# pins the kept revision of each variant against the kernel's own UCODE_API_MIN/MAX so that
# arrives as a failed build rather than as a laptop with no wlan0.
#
# Not fatal when there are none. Wireless is optional and a build with
# BR2_PACKAGE_LINUX_FIRMWARE_IWLWIFI_* turned off is a legitimate build; a machine with no
# Intel wireless simply never asks.
install_wifi_firmware() {
    local from="${TARGET_DIR}/usr/lib/firmware"
    local total=0
    local count=0
    local blob base variant revision

    # variant -> highest revision seen. A name that does not parse is keyed whole with an
    # empty revision and shipped as it is: one this cannot read is one to carry rather
    # than to drop, since dropping it is the silent half of the failure.
    local -A newest=()
    for blob in "${from}"/iwlwifi-*.ucode; do
        [ -e "${blob}" ] || continue
        base="$(basename "${blob}")"
        if [[ "${base}" =~ ^(iwlwifi-.+)-([0-9]+)\.ucode$ ]]; then
            variant="${BASH_REMATCH[1]}"
            revision="${BASH_REMATCH[2]}"
            if [ -z "${newest[${variant}]:-}" ] || [ "${revision}" -gt "${newest[${variant}]}" ]; then
                newest["${variant}"]="${revision}"
            fi
        else
            newest["${base}"]=""
        fi
    done

    for variant in "${!newest[@]}"; do
        revision="${newest[${variant}]}"
        if [ -n "${revision}" ]; then
            blob="${from}/${variant}-${revision}.ucode"
        else
            blob="${from}/${variant}"
        fi
        [ -e "${blob}" ] || continue
        install -D -m 0444 "${blob}" "${WORK}/initrd/lib/firmware/$(basename "${blob}")"
        total=$(( total + $(stat -Lc %s "${blob}") ))
        count=$(( count + 1 ))
    done

    # The AX210 family wants a platform NVM file beside its ucode, and the device does not
    # come up without one. These are not `.ucode`, so a glob written when the image carried
    # only 9000-series firmware ships an AX211 that loads its firmware and still associates
    # with nothing -- the same missing-file failure, one directory further on. They are
    # 28 to 56 KiB each, so all of them are carried rather than matched to a card.
    for blob in "${from}"/*.pnvm; do
        [ -e "${blob}" ] || continue
        install -D -m 0444 "${blob}" "${WORK}/initrd/lib/firmware/$(basename "${blob}")"
        total=$(( total + $(stat -Lc %s "${blob}") ))
        count=$(( count + 1 ))
    done

    # The regulatory database. cfg80211 asks for it as firmware when the first wireless
    # device registers, which is also during initcalls; without it the kernel falls back to
    # the world domain, which is legal everywhere and quieter and weaker than the card can
    # be. Both files or neither: the signature is what the kernel checks it by.
    for blob in "${from}"/regulatory.db "${from}"/regulatory.db.p7s; do
        [ -e "${blob}" ] || continue
        install -D -m 0444 "${blob}" "${WORK}/initrd/lib/firmware/$(basename "${blob}")"
        total=$(( total + $(stat -Lc %s "${blob}") ))
        count=$(( count + 1 ))
    done

    if [ "${count}" -gt 0 ]; then
        msg "  wireless firmware in initrd: ${count} files, $(( total / 1024 )) KiB"
    else
        msg "  no wireless firmware found -- wlan interfaces will not appear"
    fi
}

build_initrd() {
    msg "building initrd"
    local init="${TARGET_DIR}/usr/bin/plexos-init"

    [ -x "${init}" ] || die \
        "no plexos-init in the target at ${init}" \
        "package/plexos-init/ does not exist yet — it is the next thing to write (CLAUDE.md). Until it does, this script can build every artifact except a bootable UKI."

    # A dynamic binary here boots to a panic, and the panic names a missing loader
    # rather than the real cause, so it is worth refusing early and explicitly.
    #
    # Tested by looking for a PT_INTERP program header, which names the dynamic
    # loader and exists only on dynamic executables. Matching file(1) output is the
    # obvious approach and it is a trap: +crt-static yields a *static-pie* binary,
    # which file calls "static-pie linked" rather than "statically linked".
    if readelf -l "${init}" 2>/dev/null | grep -q INTERP; then
        die "plexos-init is dynamically linked, and the initrd has no loader or libc" \
            "build it with RUSTFLAGS='-C target-feature=+crt-static' (see package/plexos-init/)"
    fi

    rm -rf "${WORK}/initrd"
    mkdir -p "${WORK}/initrd"
    install -D -m 0755 "${init}" "${WORK}/initrd/init"

    # Mount points plexos-init's boot plan expects to exist before it mounts anything.
    # Creating them here rather than at runtime keeps step one of the plan honest: it
    # mounts, it does not also have to mkdir.
    mkdir -p "${WORK}/initrd"/{dev,proc,sys,run,sysroot}

    install_gpu_firmware

    ( cd "${WORK}/initrd" && find . -print0 \
        | sort -z \
        | cpio --null --create --format=newc --quiet --reproducible ) \
        > "${WORK}/initrd.cpio"

    msg "  initrd is $(( $(stat -c %s "${WORK}/initrd.cpio") / 1024 )) KiB"
}

# --------------------------------------------------------------------------
# Secure Boot signing, for everything the firmware will launch
# --------------------------------------------------------------------------
# ADR-0004's chain begins "firmware verifies the bootloader", and until this existed only
# the UKI was ever signed. That is the half that cannot work on its own: with Secure Boot
# on, firmware refuses BOOTX64.EFI before any UKI is reached, so a machine given a signed
# image and an enrolled key would have failed at the first step with a message about the
# bootloader and nothing to suggest the UKIs were fine.
#
# Both are signed with db, and by one function, so the next thing added to the ESP is
# signed by writing one line rather than by remembering that it must be.
sign_efi() {
    local target="$1" what="$2"

    if [ -z "${PLEXOS_SB_KEY}" ] || [ -z "${PLEXOS_SB_CERT}" ]; then
        msg "  ${what}: UNSIGNED (set PLEXOS_SB_KEY and PLEXOS_SB_CERT to sign)"
        return 0
    fi

    command -v sbsign >/dev/null 2>&1 || die \
        "PLEXOS_SB_KEY is set but sbsign is not installed" \
        "apt install sbsigntool, or unset PLEXOS_SB_KEY to build an unsigned image"

    msg "  signing ${what}"
    sbsign --key "${PLEXOS_SB_KEY}" --cert "${PLEXOS_SB_CERT}" \
           --output "${target}.signed" "${target}"
    mv "${target}.signed" "${target}"

    # sbsign exits 0 having written a file whose signature is not one the firmware will
    # accept -- a mismatched key and certificate is the usual way. sbverify against the
    # same certificate is the only check available here that asks the question the
    # firmware will ask, and it costs milliseconds against a boot that fails in a setup
    # screen with no log.
    if command -v sbverify >/dev/null 2>&1; then
        sbverify --cert "${PLEXOS_SB_CERT}" "${target}" >/dev/null 2>&1 || die \
            "${what} was signed and the signature does not verify against ${PLEXOS_SB_CERT}" \
            "the key and the certificate are probably not a pair; regenerate with tools/make-secureboot-keys.sh"
    fi
}

# --------------------------------------------------------------------------
# 4. The Unified Kernel Image
# --------------------------------------------------------------------------
# A UKI is the EFI stub with .osrel, .cmdline, .linux and .initrd appended as PE
# sections, so objcopy is sufficient and systemd's ukify — which would need
# python-pefile, a package Buildroot does not carry — is not needed.
#
# Section addresses are computed from the stub rather than hardcoded. The widely
# copied recipe uses fixed offsets (.linux at 0x2000000, .initrd at 0x3000000), which
# silently corrupts the image the day the kernel grows past the gap between them.
next_section_offset() {
    # Highest end address across the stub's existing sections, aligned up. objdump
    # prints size and VMA as hex in columns 3 and 4.
    objdump -h "$1" | awk '
        /^[ ]*[0-9]+ / {
            end = strtonum("0x" $3) + strtonum("0x" $4)
            if (end > max) max = end
        }
        END {
            align = 4096
            printf "%d\n", int((max + align - 1) / align) * align
        }'
}

build_uki() {
    local slot="${1:-a}"
    local out="${2:-${WORK}/plexos.efi}"

    msg "assembling Unified Kernel Image for slot ${slot}"

    # The root hash rides on the command line, inside the signed artifact. This single
    # line is what makes the UKI signature transitively cover every byte of /usr
    # (ADR-0004), and it is why this stage cannot run before the verity stage.
    # earlycon=efifb is what makes an early panic visible on a machine with no serial
    # port. It uses the framebuffer the firmware already set up, so it works before
    # any driver loads -- which is exactly the window where a boot failure is
    # otherwise silent and the only symptom is a blinking Caps Lock.
    # Console order matters, and it is not stylistic. Kernel messages go to every
    # console listed, but userspace -- including every diagnostic plexos-init prints
    # -- goes only to /dev/console, and the kernel associates that with the LAST
    # console= given (Documentation/admin-guide/serial-console.rst).
    #
    # With tty0 first and ttyS0 last, /dev/console is the serial port. On a machine
    # with no serial port that is a console nobody can read: the kernel's own messages
    # appear on screen, plexos-init's do not, and a failing boot looks like a panic
    # with no explanation. That is exactly what the first hardware boots did, and
    # QEMU could never have shown it -- there ttyS0 is the port being captured.
    #
    # tty0 last. The screen is the console an appliance actually has.
    # fbcon=font:TER16x32 -- see linux.fragment. Overridable at the boot menu by
    # pressing 'e', which is why the smaller fonts are compiled in too.
    # i915.enable_guc=2 -- HuC load, without GuC submission. Not a tuning knob: on this
    # hardware it is the difference between HuC running and not, and nothing else turns
    # it on.
    #
    # The parameter defaults to -1, "auto", which sounds like it would do the right
    # thing. uc_expand_default_options() in drivers/gpu/drm/i915/gt/uc/intel_uc.c opens
    # with:
    #
    #     /* Don't enable GuC/HuC on pre-Gen12 */
    #     if (GRAPHICS_VER(i915) < 12) { i915->params.enable_guc = 0; return; }
    #
    # Whiskey Lake-U is Gen9.5. Auto therefore means off, the driver never requests the
    # firmware, and having shipped the blobs makes no difference whatever. That was
    # measured on the reference laptop: with the firmware in the initrd and this
    # parameter absent, /api/gpu still reported guc=not_running, huc=not_running.
    #
    # 2 is ENABLE_GUC_LOAD_HUC (BIT(1) in i915_params.h). Deliberately not 3: BIT(0) is
    # GuC submission, which is a scheduling change with its own history on Gen9 and
    # buys a transcoding appliance nothing. HuC is what affects encode quality, and
    # loading it pulls in GuC anyway, because GuC is what authenticates it.
    # video=1280x720 -- the console has to be readable, and TER16x32 is the largest font
    # the kernel has. On a 2160x1440 panel that is still 135 columns of very small text,
    # which is how an `ls -la` came to be unreadable and a diagnosis had to go round by
    # experiment. Shrinking the framebuffer is the only lever left: at 1280x720 the same
    # font gives 80 columns at roughly three times the physical size.
    #
    # No connector name, so it applies to whichever output exists. A mode the panel
    # cannot do is not fatal -- DRM falls back to the preferred mode, which is today's
    # behaviour, so the worst case is no improvement rather than no picture.
    #
    # panic=20 -- without it ADR-0005 does not work, and this was missing for the whole
    # life of the project. A boot that cannot verify /usr ends in plexos-init's fail(),
    # which holds the message on screen and then returns; PID 1 returning is
    # "Attempted to kill init!", which is a panic. panic_timeout defaults to 0, and 0
    # means loop forever. So the machine sat at a panic screen with a try counter it
    # never consumed, and the "bad update undoes itself with nobody present" that
    # ADR-0005 opens with required somebody present, holding the power button, three
    # times.
    #
    # 20 seconds, not the more usual 5: an early panic -- a bad kernel, a truncated
    # initrd -- happens before fail()'s own 60-second hold exists to help, and on this
    # machine the only way to capture a panic is to photograph it. Three failed boots
    # then cost about four minutes end to end, which is the right side of the trade for
    # a path that runs when an update was already broken.
    printf 'plexos.slot=%s plexos.roothash=%s i915.enable_guc=2 panic=20 earlycon=efifb console=ttyS0,115200 console=tty0 video=1280x720 fbcon=font:TER16x32\n' \
        "${slot}" "${ROOT_HASH}" > "${WORK}/cmdline"

    # Written by stage_os_release, before the /usr image was built. See there for why
    # the ordering matters.
    [ -f "${WORK}/os-release" ] || die \
        "no os-release at ${WORK}/os-release" \
        "stage_os_release must run before build_uki; check main()"

    local stub="${BINARIES_DIR}/linuxx64.efi.stub"
    local offset
    offset=$(next_section_offset "${stub}")

    local -a args=()
    local section file size
    for pair in \
        ".osrel:${WORK}/os-release" \
        ".cmdline:${WORK}/cmdline" \
        ".linux:${BINARIES_DIR}/bzImage" \
        ".initrd:${WORK}/initrd.cpio"
    do
        section="${pair%%:*}"
        file="${pair#*:}"
        args+=( --add-section "${section}=${file}"
                --change-section-vma "${section}=$(printf '0x%x' "${offset}")" )
        size=$(stat -c %s "${file}")
        offset=$(( (offset + size + 4095) / 4096 * 4096 ))
    done

    objcopy "${args[@]}" "${stub}" "${out}"

    # Verify the sections actually landed. objcopy exits 0 when asked to add a section
    # to a PE file with no room reserved for it, and the result is a binary that the
    # firmware loads and the stub then cannot find its kernel in.
    for section in .osrel .cmdline .linux .initrd; do
        objdump -h "${out}" | grep -q " ${section}\$\| ${section} " || die \
            "section ${section} is missing from the assembled UKI" \
            "the stub may lack reserved section headers; check -Defi-stub-extra-sections in package/plexos-systemd-boot"
    done

    sign_efi "${out}" "UKI for slot ${slot}"

    msg "  UKI for slot ${slot} is $(( $(stat -c %s "${out}") / 1024 / 1024 )) MiB"
}

# --------------------------------------------------------------------------
# 5. The ESP
# --------------------------------------------------------------------------
build_esp() {
    msg "building ESP"
    local esp="${WORK}/esp.img"
    local esp_mib=512   # from LAYOUT_X86_64

    rm -f "${esp}"
    truncate -s "${esp_mib}M" "${esp}"
    "${HOST_DIR}/sbin/mkfs.vfat" -F 32 -n PLEXOS_ESP "${esp}" >/dev/null

    local -r mcopy="${HOST_DIR}/bin/mcopy"
    local -r mmd="${HOST_DIR}/bin/mmd"

    "${mmd}" -i "${esp}" ::/EFI ::/EFI/BOOT ::/EFI/systemd ::/EFI/Linux ::/loader ::/loader/entries

    # The removable-media fallback path. Firmware boots it without an NVRAM entry,
    # which is what makes the same image work from a USB stick and from an installed
    # disk without the installer having to touch EFI variables.
    # Signed into a copy, never in place: BINARIES_DIR is Buildroot's output and signing
    # what is there would make a rebuild sign an already-signed binary, which sbsign
    # refuses on the second pass and would turn a repeat build into a failure.
    local -r boot="${WORK}/systemd-bootx64.efi"
    cp "${BINARIES_DIR}/systemd-bootx64.efi" "${boot}"
    sign_efi "${boot}" "bootloader"

    "${mcopy}" -i "${esp}" "${boot}" ::/EFI/BOOT/BOOTX64.EFI
    "${mcopy}" -i "${esp}" "${boot}" ::/EFI/systemd/systemd-bootx64.efi

    # The try counter lives in the filename (ADR-0005): "+3" means three attempts
    # remain and none has been used. systemd-boot decrements it by renaming before
    # handing off, and plexosd drops the suffix entirely once the health gate passes.
    "${mcopy}" -i "${esp}" "${WORK}/plexos.efi" "::/EFI/Linux/plexos-${PLEXOS_VERSION}+3.efi"

    # console-mode 0 is the 80x25 text mode, which firmware scales up to fill the
    # panel -- the largest and most readable menu available. 'max' would pick the
    # highest resolution the firmware offers, which is the opposite of what a 2160x1440
    # laptop panel needs. editor yes allows the kernel command line to be edited at the
    # menu, which is how fbcon=font: can be changed without a rebuild.
    printf 'timeout 3\ndefault plexos-*\nconsole-mode 0\neditor yes\n' > "${WORK}/loader.conf"
    "${mcopy}" -i "${esp}" "${WORK}/loader.conf" ::/loader/loader.conf

    msg "  ESP populated"
}

# --------------------------------------------------------------------------
# 6. The disk image
# --------------------------------------------------------------------------
# Partition geometry comes from plexos-types via plexos-layout. Not one GUID, size,
# or label is written here by hand — that is the rule in buildroot/README.md, and it
# exists because the installer, the updater and plexos-init all have to agree with
# this script exactly.
build_disk() {
    msg "building disk image"

    local layout_bin="${HOST_DIR}/bin/plexos-layout"

    # rustup is commonly installed without modifying PATH, and Buildroot runs this
    # script with its own PATH regardless, so looking only at PATH finds nothing on
    # a machine that builds the workspace perfectly well. package/plexos-init does
    # the same lookup for the same reason.
    local cargo
    cargo="$(command -v cargo 2>/dev/null || true)"
    [ -n "${cargo}" ] || [ ! -x "${HOME}/.cargo/bin/cargo" ] || cargo="${HOME}/.cargo/bin/cargo"
    local -a sgdisk_args=()
    if [ -x "${layout_bin}" ]; then
        mapfile -t sgdisk_args < <("${layout_bin}" --format sgdisk)
    elif [ -n "${cargo}" ] && [ -f "${REPO_ROOT}/Cargo.toml" ]; then
        # Bring-up path: no host package for the emitter yet, but the workspace is
        # right there. Explicit rather than silent, because it makes the image build
        # depend on a toolchain Buildroot does not manage.
        msg "  (building plexos-layout from the workspace; no host package yet)"
        mapfile -t sgdisk_args < <(
            "${cargo}" run --quiet --manifest-path "${REPO_ROOT}/Cargo.toml" \
                  -p plexos-types --bin plexos-layout -- --format sgdisk
        )
    else
        die "cannot determine the partition layout" \
            "no plexos-layout in \$HOST_DIR/bin and no cargo on PATH or in \$HOME/.cargo/bin; install the Rust toolchain (docs/DEVELOPMENT.md)"
    fi

    [ "${#sgdisk_args[@]}" -gt 0 ] || die \
        "plexos-layout produced no arguments" \
        "run 'plexos-layout --format sgdisk' by hand and check its output"

    rm -f "${IMAGE}"
    truncate -s "${PLEXOS_IMAGE_SIZE}" "${IMAGE}"
    "${HOST_DIR}/sbin/sgdisk" "${sgdisk_args[@]}" "${IMAGE}" >/dev/null

    # /var is the only partition whose size is not known until the table exists, since
    # it takes the remainder of whatever disk it lands on.
    local var_start var_end var_sectors
    var_start=$(partition_start 6)
    var_end=$("${HOST_DIR}/sbin/sgdisk" -i 6 "${IMAGE}" | awk '/^Last sector:/ { print $3 }')
    var_sectors=$(( var_end - var_start + 1 ))

    msg "  formatting /var as XFS ($(( var_sectors / 2048 )) MiB)"
    rm -f "${WORK}/var.img"
    truncate -s "$(( var_sectors * 512 ))" "${WORK}/var.img"
    mkfs.xfs -q -f -L plexos-var "${WORK}/var.img" >/dev/null

    write_partition 1 "${WORK}/esp.img"
    write_partition 2 "${WORK}/usr.erofs"
    write_partition 3 "${WORK}/usr.hash"
    # Slot B is deliberately left empty. A first image populates one slot; the other
    # is written by the first update, and an empty slot is what the boot counter falls
    # back *from*, never *to*.
    write_partition 6 "${WORK}/var.img"

    msg "  ${IMAGE}"
}

partition_start() {
    "${HOST_DIR}/sbin/sgdisk" -i "$1" "${IMAGE}" | awk '/^First sector:/ { print $3 }'
}

write_partition() {
    local number="$1" source="$2" start
    start=$(partition_start "${number}")
    [ -n "${start}" ] || die "could not read the start sector of partition ${number}" \
        "check that sgdisk wrote the partition table"
    # Seeking in MiB rather than sectors, which is only correct while every partition
    # is 1 MiB aligned. sgdisk aligns to 2048 sectors by default and the layout does
    # not override it, but a silently misaligned partition would write /var over the
    # verity tree, so it is worth refusing rather than assuming.
    [ $(( start % 2048 )) -eq 0 ] || die \
        "partition ${number} starts at sector ${start}, which is not 1 MiB aligned" \
        "sgdisk's default alignment has changed; write_partition seeks in MiB and must be revisited"

    # conv=sparse skips runs of zeros instead of writing them. A freshly formatted
    # /var is several GiB of almost entirely zeros, and writing them out in full makes
    # the image fully allocated and slow to produce — for no gain, since the image was
    # created by truncate and every target region is already zero. Without this, a
    # default 8 GiB image writes ~5.4 GiB of zeros through /tmp.
    dd if="${source}" of="${IMAGE}" bs=1M seek="$(( start / 2048 ))" \
       conv=notrunc,sparse status=none
}

# --------------------------------------------------------------------------

# --------------------------------------------------------------------------
# 7. The update bundle
# --------------------------------------------------------------------------
# Everything an already-installed appliance needs to replace its /usr over the network,
# so that a new build stops meaning writing a USB stick. plexos-update consumes this.
#
# Two UKIs, differing only in `plexos.slot=`. The appliance writes whichever slot it is
# not running from and installs the matching entry; it cannot build one itself, because
# that needs objcopy and objcopy is not in the image.
#
# update.json describes the bundle and is not what an appliance installs from. Two things
# read it: tools/sign-bundle.sh, which turns it into the signed ADR-0006 manifest, and
# appliances built before signing existed, which is the only reason it is still written at
# all. Nothing signs it, and this release's updater does not parse it.
#
# The signing step is separate from the build on purpose. The key must not have to be on
# every host with a Buildroot tree, and a manifest must be written exactly once -- the
# signature covers its bytes, so a second tool that reformats it breaks it.
build_bundle() {
    local bundle="${BINARIES_DIR}/plexos-update"
    msg "building update bundle"

    rm -rf "${bundle}"
    mkdir -p "${bundle}"

    cp "${WORK}/usr.erofs" "${bundle}/usr.erofs"
    cp "${WORK}/usr.hash"  "${bundle}/usr.hash"

    # Slot A's UKI already exists -- it is the one written to the ESP. Slot B's differs
    # by one word on the kernel command line and is built here rather than patched,
    # because patching a PE section in place is how a UKI stops matching its own hashes.
    cp "${WORK}/plexos.efi" "${bundle}/plexos-${PLEXOS_VERSION}-a.efi"
    build_uki b "${bundle}/plexos-${PLEXOS_VERSION}-b.efi"

    local usr_size verity_size uki_a_size uki_b_size
    local usr_sum verity_sum uki_a_sum uki_b_sum
    usr_size=$(stat -c %s "${bundle}/usr.erofs")
    verity_size=$(stat -c %s "${bundle}/usr.hash")
    uki_a_size=$(stat -c %s "${bundle}/plexos-${PLEXOS_VERSION}-a.efi")
    uki_b_size=$(stat -c %s "${bundle}/plexos-${PLEXOS_VERSION}-b.efi")
    usr_sum=$(sha256sum "${bundle}/usr.erofs" | cut -d' ' -f1)
    verity_sum=$(sha256sum "${bundle}/usr.hash" | cut -d' ' -f1)
    uki_a_sum=$(sha256sum "${bundle}/plexos-${PLEXOS_VERSION}-a.efi" | cut -d' ' -f1)
    uki_b_sum=$(sha256sum "${bundle}/plexos-${PLEXOS_VERSION}-b.efi" | cut -d' ' -f1)

    cat > "${bundle}/update.json" <<JSON
{
  "bundle_version": 1,
  "version": "${PLEXOS_VERSION}",
  "root_hash": "${ROOT_HASH}",
  "usr":    { "name": "usr.erofs", "size": ${usr_size}, "sha256": "${usr_sum}" },
  "verity": { "name": "usr.hash", "size": ${verity_size}, "sha256": "${verity_sum}" },
  "uki_a":  { "name": "plexos-${PLEXOS_VERSION}-a.efi", "size": ${uki_a_size}, "sha256": "${uki_a_sum}" },
  "uki_b":  { "name": "plexos-${PLEXOS_VERSION}-b.efi", "size": ${uki_b_size}, "sha256": "${uki_b_sum}" }
}
JSON

    # The slot each UKI actually boots, checked rather than assumed. A bundle whose two
    # entries carry the same plexos.slot= would write slot B and then boot slot A from
    # it, which dm-verity refuses with a root hash mismatch -- a failure that looks like
    # a corrupt download and is not.
    local found
    for slot in a b; do
        found=$(strings "${bundle}/plexos-${PLEXOS_VERSION}-${slot}.efi" \
                | grep -o 'plexos\.slot=[ab]' | head -1)
        [ "${found}" = "plexos.slot=${slot}" ] || die \
            "the UKI for slot ${slot} carries ${found:-no plexos.slot at all}" \
            "build_uki was called with the wrong slot, or the command line is not in .cmdline"
    done

    msg "  bundle at ${bundle} ($(( (usr_size + verity_size + uki_a_size + uki_b_size) / 1024 / 1024 )) MiB)"
    msg "  version ${PLEXOS_VERSION}, root hash ${ROOT_HASH}"
}

main() {
    preflight
    rm -rf "${WORK}"
    mkdir -p "${WORK}"

    # Before build_usr_image, not merely somewhere earlier: the factory /etc it stages
    # is a copy of TARGET_DIR/etc, so the accounts have to be there first or they are
    # absent from the image. build_usr_image checks that they are, so swapping these
    # two fails the build rather than shipping a system with a nameless uid.
    apply_users_table
    stage_os_release

    build_usr_image
    build_verity
    build_initrd
    build_uki
    build_esp
    build_disk
    build_bundle

    msg "done"
    msg "  write it with: sudo dd if=${IMAGE} of=/dev/sdX bs=4M status=progress conv=fsync"
    msg "  Secure Boot must be off in firmware unless the UKI was signed"
}

# Run only when executed. Sourcing the script exposes the individual stages so they
# can be tested against real tools without assembling a whole image, which is how the
# erofs, verity and GPT stages were verified before a Buildroot build existed to run
# them for real. Buildroot executes this file, so main() still runs there.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
