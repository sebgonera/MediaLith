#!/usr/bin/env bash
#
# Tests for post-image.sh.
#
# Image assembly is the one part of PlexOS whose mistakes are both silent and
# expensive: a wrong section offset, a non-reproducible filesystem, or a partition
# written at the wrong sector all produce an artifact that looks entirely normal and
# fails only on a machine that will not boot. Waiting for a four-hour Buildroot build
# to find out is not a workable feedback loop.
#
# So this runs post-image.sh's stages directly, against real tools, on a mock target
# tree that takes seconds to build. post-image.sh only calls main() when executed, so
# sourcing it here exposes the stages individually.
#
# Stages whose tools Buildroot has not built yet are SKIPPED, not failed. That means
# this can be run at any point during bring-up and simply covers more as the build
# progresses. A skip is reported loudly, because a silent skip reads as a pass.
#
#   ./post-image-test.sh [buildroot-output-dir]
#
# The output directory defaults to $PLEXOS_OUTPUT, then to ./output. Tools are taken
# from its host/ tree where they exist and from the system otherwise.

set -uo pipefail

BOARD_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT="${1:-${PLEXOS_OUTPUT:-$(cd "${BOARD_DIR}/../../../.." && pwd)/output}}"
BR_HOST="${OUTPUT}/host"

pass=0; fail=0; skip=0
ok()   { printf '  ok    %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '  FAIL  %s\n' "$1"; [ $# -gt 1 ] && printf '        %s\n' "$2"; fail=$((fail + 1)); }
skipped() { printf '  skip  %s\n' "$1"; [ $# -gt 1 ] && printf '        %s\n' "$2"; skip=$((skip + 1)); }
stage() { printf '\n== %s ==\n' "$1"; }

check()  { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "got '$2', want '$3'"; fi; }
assert() { if eval "$2"; then ok "$1"; else bad "$1" "${3:-}"; fi; }

# Resolve a tool from Buildroot's host tree first, then the system. Buildroot's copy
# is the one a real build uses, and it is pinned with the rest of the build.
find_tool() {
    local name="$1" candidate
    for candidate in "${BR_HOST}/bin/${name}" "${BR_HOST}/sbin/${name}"; do
        [ -x "${candidate}" ] && { printf '%s\n' "${candidate}"; return 0; }
    done
    command -v "${name}" 2>/dev/null && return 0
    return 1
}

# Scratch goes next to the Buildroot output, not in $TMPDIR. A disk image is
# gigabytes, and /tmp is commonly a tmpfs sized well under that — on the development
# machine it is a 3.6 GiB tmpfs with a per-user quota, and filling it took down a
# concurrent Buildroot build by starving GCC of space for its temporary assembly.
# The output directory is by definition on a filesystem chosen to hold a build.
if [ -d "${OUTPUT}" ] && [ -w "${OUTPUT}" ]; then
    TMP="$(mktemp -d "${OUTPUT}/.post-image-test.XXXXXX")"
else
    TMP="$(mktemp -d)"
    printf 'warning: %s is not writable; using %s, which needs several GiB free\n' \
        "${OUTPUT}" "${TMP}" >&2
fi
trap 'rm -rf "${TMP}"' EXIT

# Smaller than post-image.sh's default. The fixed partitions come to ~2.6 GiB
# (ADR-0003), so this still leaves a real /var to format, while keeping the test
# cheap enough to run in a tight loop.
export PLEXOS_IMAGE_SIZE="${PLEXOS_IMAGE_SIZE:-4G}"

MKFS_EROFS="$(find_tool mkfs.erofs || true)"
FSCK_EROFS="$(find_tool fsck.erofs || true)"
VERITYSETUP="$(find_tool veritysetup || true)"
SGDISK="$(find_tool sgdisk || true)"
MKFS_VFAT="$(find_tool mkfs.vfat || true)"
MCOPY="$(find_tool mcopy || true)"

printf 'buildroot output: %s\n' "${OUTPUT}"
printf 'mkfs.erofs:  %s\n' "${MKFS_EROFS:-NOT FOUND}"
printf 'veritysetup: %s\n' "${VERITYSETUP:-NOT FOUND}"
printf 'sgdisk:      %s\n' "${SGDISK:-NOT FOUND}"
printf 'mcopy:       %s\n' "${MCOPY:-NOT FOUND}"

# --------------------------------------------------------------------------
# A mock Buildroot tree: enough shape for post-image.sh to work on, and nothing more.
# --------------------------------------------------------------------------
MOCK="${TMP}/mock"
mkdir -p "${MOCK}"/{images,host/bin,host/sbin,target/etc,target/usr/{bin,lib,share/factory/etc}}

# A skeleton /etc, because Buildroot's target always has one and mkusers appends to the
# files rather than creating them. Root only: every other account is meant to arrive
# from the users table, which is the thing under test.
printf 'root:x:0:0:root:/root:/bin/sh\n'  > "${MOCK}/target/etc/passwd"
printf 'root:x:0:\n'                      > "${MOCK}/target/etc/group"
printf 'root:*:::::::\n'                  > "${MOCK}/target/etc/shadow"
chmod 600 "${MOCK}/target/etc/shadow"

for tool in MKFS_EROFS:bin/mkfs.erofs VERITYSETUP:sbin/veritysetup SGDISK:sbin/sgdisk \
            MKFS_VFAT:sbin/mkfs.vfat MCOPY:bin/mcopy; do
    var="${tool%%:*}"; dest="${tool#*:}"
    [ -n "${!var}" ] && ln -sf "${!var}" "${MOCK}/host/${dest}"
done
# mmd ships with mcopy; post-image.sh uses both.
[ -n "${MCOPY}" ] && ln -sf "$(dirname "${MCOPY}")/mmd" "${MOCK}/host/bin/mmd"

# A /usr with enough content that the verity tree spans more than one block.
cp /bin/sh "${MOCK}/target/usr/bin/" 2>/dev/null || true
head -c 4000000 /dev/urandom > "${MOCK}/target/usr/lib/filler.bin"
printf 'NAME="PlexOS"\nID=plexos\nVERSION_ID=0.1.0\n' > "${MOCK}/target/usr/lib/os-release"

# shellcheck source=post-image.sh disable=SC1091
source "${BOARD_DIR}/post-image.sh" "${MOCK}/images"
set +e   # post-image.sh sets -e; the checks below must be allowed to fail

WORK="${MOCK}/images/plexos-work"
IMAGE="${MOCK}/images/plexos.img"
mkdir -p "${WORK}"

# --------------------------------------------------------------------------
stage "stage 0 — the users table"
# The bug this stage exists for: Buildroot applies the users table to a *copy* of the
# target while generating each filesystem image, so TARGET_DIR/etc never gains the
# accounts, and the factory /etc staged from it therefore ships without them. It cost
# nothing to build and nothing to boot -- uid 900 simply had no name on the appliance.
#
# Buildroot's real mkusers is used, not a stand-in. A stand-in would agree with whatever
# this script believed the passwd format to be, which is the failure mode described in
# CLAUDE.md: a test that only compares a thing to itself.
BR_TOPDIR="${PLEXOS_BUILDROOT_DIR:-}"
if [ -z "${BR_TOPDIR}" ] && [ -f "${OUTPUT}/Makefile" ]; then
    BR_TOPDIR="$(sed -n 's/^MAKEARGS := -C //p' "${OUTPUT}/Makefile" | head -n 1)"
fi

if [ ! -x "${BR_TOPDIR}/support/scripts/mkusers" ]; then
    skipped "the whole stage" \
        "no Buildroot source tree; set PLEXOS_BUILDROOT_DIR, or point this at an output dir built out-of-tree"
else
    PLEXOS_BUILDROOT_DIR="${BR_TOPDIR}"
    BUILD_DIR="${MOCK}/build"

    # What Buildroot merges from package-declared users and BR2_ROOTFS_USERS_TABLES.
    # With no package declaring one, that is exactly this board's table.
    mkdir -p "${BUILD_DIR}/buildroot-fs"
    cp "${BOARD_DIR}/users.table" "${BUILD_DIR}/buildroot-fs/full_users_table.txt"

    BR2_CONFIG="${MOCK}/.config"
    if [ -f "${OUTPUT}/.config" ]; then
        cp "${OUTPUT}/.config" "${BR2_CONFIG}"
    else
        printf 'BR2_TARGET_GENERIC_PASSWD_METHOD="sha-256"\n' > "${BR2_CONFIG}"
    fi

    # In a subshell throughout: post-image.sh's die() calls exit, which from this shell
    # would take the whole test run with it and lose the summary. Its effects are file
    # writes, which a subshell makes just the same.
    ( apply_users_table ) >/dev/null 2>&1
    check "mkusers runs against the target tree" "$?" "0"

    check "the account reaches the target's /etc/passwd" \
          "$(awk -F: '$1 == "plex" { print $3 ":" $4 }' "${MOCK}/target/etc/passwd")" \
          "900:900"
    check "and its group" \
          "$(awk -F: '$1 == "plex" { print $3 }' "${MOCK}/target/etc/group")" \
          "900"
    check "with the home directory ADR-0009 names" \
          "$(awk -F: '$1 == "plex" { print $6 }' "${MOCK}/target/etc/passwd")" \
          "/var/lib/plex"
    check "and no usable shell" \
          "$(awk -F: '$1 == "plex" { print $7 }' "${MOCK}/target/etc/passwd")" \
          "/bin/false"

    # Buildroot itself runs mkusers again on every rebuild, against a tree this one has
    # already written. If a second pass were an error rather than a consistency check,
    # every incremental build would fail.
    ( apply_users_table ) >/dev/null 2>&1
    check "a second pass is not an error" "$?" "0"
    check "and does not duplicate the account" \
          "$(awk -F: '$1 == "plex"' "${MOCK}/target/etc/passwd" | wc -l)" \
          "1"
fi

# Stage 1 stages the factory /etc and refuses to build an image without the declared
# accounts in it. When stage 0 was skipped there is no mkusers to have put them there,
# so they are written by hand — which keeps the stages below running on a machine with
# no Buildroot checkout, at the cost of proving nothing about mkusers itself. Said out
# loud, because a quiet substitution here would read as coverage that does not exist.
if ! awk -F: '$1 == "plex"' "${MOCK}/target/etc/passwd" | grep -q .; then
    printf 'plex:x:900:900:Plex Media Server:/var/lib/plex:/bin/false\n' \
        >> "${MOCK}/target/etc/passwd"
    printf 'plex:x:900:\n' >> "${MOCK}/target/etc/group"
    printf 'note: accounts written by hand for the stages below; stage 0 was skipped\n'
fi

# --------------------------------------------------------------------------
stage "stage 1 — /usr erofs"
if [ -z "${MKFS_EROFS}" ]; then
    skipped "the whole stage" "mkfs.erofs not built; enable BR2_TARGET_ROOTFS_EROFS and build"
else
    build_usr_image >/dev/null
    assert "an image is produced" "[ -s '${WORK}/usr.erofs' ]"
    # erofs superblock lives at offset 1024 and starts with magic 0xe0f5e1e2.
    magic=$(od -An -tx4 -j1024 -N4 "${WORK}/usr.erofs" | tr -d ' ')
    check "erofs superblock magic" "${magic}" "e0f5e1e2"

    # Reproducibility is not a nicety here. If two builds of identical inputs differ,
    # the verity root hash differs, and with it the signed command line — so there is
    # no way to confirm a released image corresponds to a given commit. mkfs.erofs
    # randomises the superblock UUID unless -U is passed, which is exactly the kind of
    # difference that hides until someone tries to reproduce a release.
    cp "${WORK}/usr.erofs" "${WORK}/first.erofs"
    build_usr_image >/dev/null
    check "a rebuild is byte-identical" \
          "$(sha256sum < "${WORK}/usr.erofs" | cut -d' ' -f1)" \
          "$(sha256sum < "${WORK}/first.erofs" | cut -d' ' -f1)"

    check "the account is in the staged factory /etc" \
          "$(awk -F: '$1 == "plex" { print $3 }' \
             "${MOCK}/target/usr/share/factory/etc/passwd")" \
          "900"

    # The check that would have caught the original bug, and the only one here that
    # inspects the artifact a machine actually boots rather than a tree on the way to
    # it. Everything upstream of the image can be right while the image is wrong.
    if [ -z "${FSCK_EROFS}" ]; then
        skipped "the account survives into the image" "fsck.erofs not available to extract it"
    else
        rm -rf "${TMP}/extract"
        mkdir -p "${TMP}/extract"
        "${FSCK_EROFS}" --extract="${TMP}/extract" --overwrite "${WORK}/usr.erofs" \
            >/dev/null 2>&1
        check "the account survives into the image" \
              "$(awk -F: '$1 == "plex" { print $3 ":" $6 }' \
                 "${TMP}/extract/share/factory/etc/passwd" 2>/dev/null)" \
              "900:/var/lib/plex"
    fi

    # And the guard itself, which is worth nothing if it cannot fail. Without this the
    # two checks above would keep passing against a check_factory_accounts that had
    # been quietly turned into a no-op.
    cp "${MOCK}/target/etc/passwd" "${TMP}/passwd.keep"
    grep -v '^plex:' "${TMP}/passwd.keep" > "${MOCK}/target/etc/passwd"
    ( build_usr_image ) >/dev/null 2>&1
    check "an image without the account is refused" "$?" "1"
    cp "${TMP}/passwd.keep" "${MOCK}/target/etc/passwd"

    # Left as the successful build found it, since the stages below verify this image.
    build_usr_image >/dev/null
fi

# --------------------------------------------------------------------------
stage "stage 2 — dm-verity"
if [ -z "${VERITYSETUP}" ] || [ ! -s "${WORK}/usr.erofs" ]; then
    skipped "the whole stage" "needs veritysetup and a /usr image from stage 1"
else
    build_verity >/dev/null
    assert "a hash tree is produced" "[ -s '${WORK}/usr.hash' ]"
    check "root hash is 64 characters" "${#ROOT_HASH}" "64"
    assert "root hash is lowercase hex" "[[ '${ROOT_HASH}' =~ ^[0-9a-f]{64}$ ]]"

    first_hash="${ROOT_HASH}"
    build_verity >/dev/null
    check "the fixed salt gives a stable root hash" "${ROOT_HASH}" "${first_hash}"

    # The point of the tree is that it detects modification of the data it covers.
    # Asserting it exists proves nothing; this proves it works.
    assert "veritysetup verifies the tree we built" \
           "'${VERITYSETUP}' verify '${WORK}/usr.erofs' '${WORK}/usr.hash' '${ROOT_HASH}' >/dev/null 2>&1"

    cp "${WORK}/usr.erofs" "${WORK}/tampered.erofs"
    printf 'x' | dd of="${WORK}/tampered.erofs" bs=1 seek=2000000 conv=notrunc status=none
    assert "one flipped byte fails verification" \
           "! '${VERITYSETUP}' verify '${WORK}/tampered.erofs' '${WORK}/usr.hash' '${ROOT_HASH}' >/dev/null 2>&1" \
           "the hash tree does not actually cover the image data"
fi

# --------------------------------------------------------------------------
stage "stage 3 — GPU firmware in the initrd"
# i915 is asserted by post-image.sh itself, which dies without it. `xe` is not, because a
# build for a machine with no such GPU is legitimate — so the enforcement is here, and it
# exists because four options once vanished from a defconfig without kconfig saying a word.
XE_SRC="${OUTPUT}/target/usr/lib/firmware/xe"
if [ ! -d "${XE_SRC}" ]; then
    skipped "the whole stage" \
            "no ${XE_SRC}; set BR2_PACKAGE_LINUX_FIRMWARE_XE and rebuild linux-firmware"
else
    saved_work="${WORK}"; saved_target="${TARGET_DIR:-}"
    WORK="${TMP}/xe"; TARGET_DIR="${OUTPUT}/target"
    mkdir -p "${WORK}"
    install_xe_firmware >/dev/null
    WORK="${saved_work}"; TARGET_DIR="${saved_target}"
    XE_GOT="${TMP}/xe/initrd/lib/firmware/xe"

    # The subdirectory is the point. xe_uc_fw.c builds the request as
    # `xe/<plat>_guc_<major>.bin`, so a blob copied flat into lib/firmware is a blob the
    # driver asks for by a different name and never finds -- and it carries on without it,
    # which is the silent half of the failure.
    assert "the blobs keep their xe/ subdirectory" "[ -d '${XE_GOT}' ]" \
           "xe asks for xe/<plat>_guc_<major>.bin; a flat copy is never found"

    for want in guc huc; do
        assert "some ${want} firmware is carried" \
               "ls '${XE_GOT}'/*_${want}*.bin >/dev/null 2>&1" \
               "without it an Arc or Xe2 part binds and runs at reduced quality"
    done

    # Neither GuC nor HuC, and both would be dropped by a pattern written for those two.
    # The GSC is what HuC authentication goes through on these parts.
    assert "the GSC blob is carried too" "ls '${XE_GOT}'/*gsc*.bin >/dev/null 2>&1" \
           "a glob for guc and huc alone drops it, which is the firmware-list mistake again"

    src=$(find "${XE_SRC}" -maxdepth 1 -type f | wc -l)
    got=$(find "${XE_GOT}" -maxdepth 1 -type f 2>/dev/null | wc -l)
    check "every blob linux-firmware provided reaches the initrd" "${got}" "${src}"
fi

# --------------------------------------------------------------------------
stage "stage 3 — wireless firmware in the initrd"
# This stage had no test at all, which is how a firmware list naming the wrong family
# survived a build, a boot and a clean run of this file. Every failure it can produce wears
# the same disguise -- a card that lspci names correctly and that registers no netdev -- so
# the checks are about the *set* of blobs rather than about any single one of them.
FW_SRC="${OUTPUT}/target/usr/lib/firmware"
KCFG="$(ls -d "${OUTPUT}"/build/linux-*/drivers/net/wireless/intel/iwlwifi/cfg 2>/dev/null | head -1)"
if ! ls "${FW_SRC}"/iwlwifi-*.ucode >/dev/null 2>&1; then
    skipped "the whole stage" "needs linux-firmware installed into ${OUTPUT}/target"
else
    saved_work="${WORK}"; saved_target="${TARGET_DIR:-}"
    WORK="${TMP}/fw"; TARGET_DIR="${OUTPUT}/target"
    mkdir -p "${WORK}"
    install_wifi_firmware >/dev/null
    WORK="${saved_work}"; TARGET_DIR="${saved_target}"
    GOT="${TMP}/fw/initrd/lib/firmware"

    assert "firmware reaches the initrd at all" "ls '${GOT}'/iwlwifi-*.ucode >/dev/null 2>&1" \
           "blobs in /usr are blobs iwlwifi never sees; it probes before /usr is mounted"

    # The pruning. Two revisions of one variant means the selection stopped working, and
    # the cost is paid twice per bundle rather than reported.
    dupes=$(ls "${GOT}"/iwlwifi-*.ucode 2>/dev/null | xargs -n1 basename \
            | sed 's/-[0-9]\+\.ucode$//' | sort | uniq -d | tr '\n' ' ')
    check "one API revision of each variant is carried" "${dupes:-none}" "none"

    # And the half that pruning can get wrong. iwlwifi asks for revisions from its
    # UCODE_API_MAX downwards, so a blob numbered above every MAX this kernel defines is
    # one it will never open -- and keeping only that one puts the card back to having no
    # firmware, on an image whose firmware directory is visibly not empty. The code that
    # changes when this fails is install_wifi_firmware: keep the newest revision the
    # kernel still asks for, not the newest that exists.
    #
    # The bound is the highest MAX across all families rather than the one belonging to
    # each variant, so what it catches is linux-firmware outrunning the kernel outright.
    # A single family running ahead while another lags would pass. Pinning per family
    # needs a variant-to-cfg table, and a hardcoded table that drifts is the failure this
    # is trying to prevent rather than a stricter version of it.
    if [ -z "${KCFG}" ]; then
        skipped "revisions are ones this kernel asks for" "no kernel source under ${OUTPUT}/build"
    else
        api_max=$(grep -h 'UCODE_API_MAX' "${KCFG}"/*.c 2>/dev/null | grep -oE '[0-9]+$' | sort -n | tail -1)
        too_new=$(for f in "${GOT}"/iwlwifi-*.ucode; do
                      r=$(basename "${f}" | sed -n 's/.*-\([0-9]\+\)\.ucode$/\1/p')
                      [ -n "${r}" ] && [ "${r}" -gt "${api_max:-0}" ] && basename "${f}"
                  done | tr '\n' ' ')
        check "no blob is numbered above any API this kernel asks for (max ${api_max:-?})" \
              "${too_new:-none}" "none"
    fi

    # The families themselves. 6E is the one that is easy to leave out and the one an
    # Alder Lake machine needs; its blobs are named for the silicon rather than for the
    # product, so nothing about "AX211" appears anywhere in the filename.
    assert "the AX210 family is covered (AX210, AX211)" \
           "ls '${GOT}'/iwlwifi-*-gf-a0-*.ucode >/dev/null 2>&1" \
           "set BR2_PACKAGE_LINUX_FIRMWARE_IWLWIFI_6E; without it those laptops have no wlan0"
    assert "and the PNVM those parts need" "ls '${GOT}'/*.pnvm >/dev/null 2>&1" \
           "not a .ucode, so a glob written for ucode alone drops it and the card associates with nothing"

    # Both files or neither: the signature is what the kernel checks the database by.
    assert "the regulatory database is carried whole" \
           "[ -e '${GOT}/regulatory.db' ] && [ -e '${GOT}/regulatory.db.p7s' ]" \
           "without it the kernel falls back to the world domain, which is weaker than the card"

    # This rides in both UKIs and in every update bundle, so it is charged four times over.
    # At 70 MiB it took the UKI to 112 MiB against the 128 MiB budget partition.rs asserts
    # the ESP against, and three of those on a 512 MiB ESP is the failure already in the
    # trap list. The bound is generous; what it catches is the pruning silently stopping.
    kib=$(du -Lsk "${GOT}" | cut -f1)
    assert "the set stays inside its budget (${kib} KiB)" "[ '${kib}' -lt 32768 ]" \
           "grew past 32 MiB: check install_wifi_firmware still keeps one revision per variant"
fi

# --------------------------------------------------------------------------
stage "stage 0b — the version stamp"
# build_uki reads ${WORK}/os-release, which stage_os_release writes before the /usr image
# is built. Calling build_uki without it used to work and now dies -- deliberately, since
# that ordering is what kept /usr/lib/os-release saying "Buildroot 2026.02.3" while the
# boot entry said something else. The test has to follow the same order the build does.
stage_os_release >/dev/null
assert "an os-release is written" "[ -s '${WORK}/os-release' ]"
check "it carries the PlexOS version rather than Buildroot's" \
      "$(sed -n 's/^VERSION_ID=//p' "${WORK}/os-release")" \
      "${PLEXOS_VERSION}"
assert "and the same file reaches the image tree" \
       "[ -s '${MOCK}/target/usr/lib/os-release' ]"
check "with the same version in it" \
      "$(sed -n 's/^VERSION_ID=//p' "${MOCK}/target/usr/lib/os-release")" \
      "${PLEXOS_VERSION}"

# The stamp is not cosmetic any more. It is the manifest's anti-rollback sequence
# (ADR-0006) and the string systemd-boot orders entries by, so an image built without one
# is an image that cannot be updated to and cannot refuse a replayed release. Neither
# failure is visible from the outside: both look like an update that did nothing.
assert "the version carries a YYYYMMDDHHMM build stamp" \
       "printf '%s' \"${PLEXOS_VERSION}\" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]{12}$'" \
       "build with PLEXOS_VERSION=0.1.0.\$(date -u +%Y%m%d%H%M), or let post-image.sh default it"

# --------------------------------------------------------------------------
stage "stage 4 — Unified Kernel Image"
STUB="${MOCK}/images/linuxx64.efi.stub"
if [ -f "${OUTPUT}/images/linuxx64.efi.stub" ]; then
    cp "${OUTPUT}/images/linuxx64.efi.stub" "${STUB}"
elif [ -f /usr/lib/systemd/boot/efi/linuxx64.efi.stub ]; then
    cp /usr/lib/systemd/boot/efi/linuxx64.efi.stub "${STUB}"
fi

if [ ! -f "${STUB}" ]; then
    skipped "the whole stage" "no linuxx64.efi.stub; build package/plexos-systemd-boot, or apt install systemd-boot-efi"
elif [ -z "${ROOT_HASH:-}" ]; then
    skipped "the whole stage" "needs a root hash from stage 2"
else
    # A kernel-shaped stand-in is enough: this stage cares about PE section layout,
    # not about the kernel being bootable.
    cp /boot/vmlinuz "${MOCK}/images/bzImage" 2>/dev/null \
        || head -c 12000000 /dev/urandom > "${MOCK}/images/bzImage"
    head -c 2000000 /dev/urandom > "${WORK}/initrd.cpio"

    build_uki >/dev/null 2>&1
    assert "a UKI is produced" "[ -s '${WORK}/plexos.efi' ]"

    for section in .osrel .cmdline .linux .initrd; do
        assert "section ${section} is present" \
               "objdump -h '${WORK}/plexos.efi' | grep -q ' ${section}'"
    done

    # The failure this guards against is overlap: the widely copied recipe hardcodes
    # .linux at 0x2000000 and .initrd at 0x3000000, which silently corrupts the image
    # once the kernel exceeds the gap between them.
    overlap=$(objdump -h "${WORK}/plexos.efi" | awk '
        /^[ ]*[0-9]+ / {
            start = strtonum("0x" $4); end = start + strtonum("0x" $3)
            for (i in s) if (start < e[i] && end > s[i]) { print "overlap"; exit }
            s[NR] = start; e[NR] = end
        }')
    check "no two sections overlap" "${overlap:-none}" "none"

    # The root hash must be inside the signed artifact, because that is what makes the
    # signature transitively cover /usr (ADR-0004).
    assert "the root hash is embedded in .cmdline" \
           "objcopy -O binary --only-section=.cmdline '${WORK}/plexos.efi' '${TMP}/cmdline.bin' 2>/dev/null && grep -q '${ROOT_HASH}' '${TMP}/cmdline.bin'"

    # ADR-0005 requires a failed boot to consume a try, and a try is consumed by
    # booting -- so a boot that ends in a panic has to reboot rather than sit there.
    # panic_timeout defaults to 0, which means loop forever, so the parameter being
    # absent is indistinguishable from the parameter being wrong. Asserted here
    # because nothing else can: the alternative is to notice on the one occasion the
    # rollback path is exercised, which is the occasion it must not fail on.
    assert "a panicking boot is told to reboot, so a try is actually consumed" \
           "objcopy -O binary --only-section=.cmdline '${WORK}/plexos.efi' '${TMP}/cmdline.bin' 2>/dev/null && grep -qE 'panic=[1-9]' '${TMP}/cmdline.bin'"
fi

# --------------------------------------------------------------------------
stage "stage 5 — ESP"
if [ -z "${MCOPY}" ] || [ -z "${MKFS_VFAT}" ]; then
    skipped "the whole stage" "needs host-mtools and host-dosfstools; build them"
elif [ ! -s "${WORK}/plexos.efi" ]; then
    skipped "the whole stage" "needs a UKI from stage 4"
else
    cp "${OUTPUT}/images/systemd-bootx64.efi" "${MOCK}/images/" 2>/dev/null \
        || cp "${WORK}/plexos.efi" "${MOCK}/images/systemd-bootx64.efi"
    build_esp >/dev/null
    assert "an ESP image is produced" "[ -s '${WORK}/esp.img' ]"
    listing=$("$(dirname "${MCOPY}")/mdir" -i "${WORK}/esp.img" -b ::/EFI/BOOT 2>/dev/null)
    assert "the removable-media fallback path exists" \
           "printf '%s' \"${listing}\" | grep -qi 'BOOTX64.EFI'" \
           "firmware boots EFI/BOOT/BOOTX64.EFI without an NVRAM entry"
    entries=$("$(dirname "${MCOPY}")/mdir" -i "${WORK}/esp.img" -b ::/EFI/Linux 2>/dev/null)
    assert "the UKI carries a boot try counter (ADR-0005)" \
           "printf '%s' \"${entries}\" | grep -q '+3'" \
           "systemd-boot decrements the counter by renaming; without it there is no rollback"

    # ADR-0004's first link. The bootloader is what firmware launches, so an image whose
    # UKIs are signed and whose BOOTX64.EFI is not cannot boot with Secure Boot on -- and
    # that was the state of this script until the signing was made one function used by
    # both. Asserted in whichever direction the build was asked for, because "unsigned"
    # is a legitimate build and silently signing nothing is the failure being prevented.
    if [ -n "${PLEXOS_SB_KEY:-}" ] && [ -n "${PLEXOS_SB_CERT:-}" ]; then
        if command -v sbverify >/dev/null 2>&1; then
            "$(dirname "${MCOPY}")/mcopy" -i "${WORK}/esp.img" \
                ::/EFI/BOOT/BOOTX64.EFI "${WORK}/bootx64.check" 2>/dev/null
            assert "the bootloader is signed, not just the UKIs" \
                   "sbverify --cert '${PLEXOS_SB_CERT}' '${WORK}/bootx64.check' >/dev/null 2>&1" \
                   "firmware refuses BOOTX64.EFI before any UKI is reached"
            assert "the UKI is signed too" \
                   "sbverify --cert '${PLEXOS_SB_CERT}' '${WORK}/plexos.efi' >/dev/null 2>&1" \
                   "systemd-boot loads it through the firmware, which checks it"
        else
            skipped "signatures on the ESP" "needs sbverify (apt install sbsigntool)"
        fi
    else
        skipped "signatures on the ESP" \
                "PLEXOS_SB_KEY is unset, so this build is deliberately unsigned"
    fi
fi

# --------------------------------------------------------------------------
stage "stage 6 — GPT and partition placement"
if [ -z "${SGDISK}" ] || [ ! -s "${WORK}/usr.erofs" ]; then
    skipped "the whole stage" "needs sgdisk and artifacts from stages 1 and 2"
else
    # Stand in for the ESP if stage 5 was skipped: this stage tests placement, and a
    # partition's contents are opaque to it.
    [ -s "${WORK}/esp.img" ] || { truncate -s 512M "${WORK}/esp.img"; }

    build_disk >/dev/null
    assert "a disk image is produced" "[ -s '${IMAGE}' ]"
    check "six partitions" "$("${SGDISK}" -p "${IMAGE}" 2>/dev/null | grep -cE '^ +[0-9]+ ')" "6"

    # Labels carry slot identity (ADR-0003), so a disk with the wrong ones is one
    # plexos-init cannot reason about.
    n=1
    for want in esp usr_a usr_a_hash usr_b usr_b_hash var; do
        got=$("${SGDISK}" -i "${n}" "${IMAGE}" | awk -F"'" '/^Partition name:/ { print $2 }')
        check "partition ${n} is named ${want}" "${got}" "${want}"
        n=$((n + 1))
    done

    # The two /usr slots must share a type GUID and differ only by label.
    a=$("${SGDISK}" -i 2 "${IMAGE}" | awk '/^Partition GUID code:/ { print $4 }')
    b=$("${SGDISK}" -i 4 "${IMAGE}" | awk '/^Partition GUID code:/ { print $4 }')
    check "both /usr slots share a type GUID" "${a}" "${b}"
    check "/usr type is the DPS value" "${a}" "8484680C-9521-48C6-9C11-B0720656F69E"

    # Content has to land at the sector the table says, or the system boots nothing.
    start=$(partition_start 2)
    magic=$(od -An -tx4 -j$(( start * 512 + 1024 )) -N4 "${IMAGE}" | tr -d ' ')
    check "the erofs lands at partition 2's offset" "${magic}" "e0f5e1e2"

    start6=$(partition_start 6)
    xfs=$(od -An -c -j$(( start6 * 512 )) -N4 "${IMAGE}" | tr -d ' \n')
    check "/var is XFS at partition 6's offset" "${xfs}" "XFSB"

    # Slot B is deliberately empty on a first image: it is what the boot counter falls
    # back *from*, never *to*.
    start4=$(partition_start 4)
    empty=$(od -An -tx1 -j$(( start4 * 512 )) -N16 "${IMAGE}" | tr -d ' ')
    check "slot B is left empty" "${empty}" "00000000000000000000000000000000"
fi

printf '\npassed %d, failed %d, skipped %d\n' "${pass}" "${fail}" "${skip}"
[ "${fail}" -eq 0 ]
