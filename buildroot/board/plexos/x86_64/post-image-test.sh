#!/usr/bin/env bash
#
# Tests for post-image.sh.
#
# Image assembly is the one part of MediaLith whose mistakes are both silent and
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
printf 'NAME="MediaLith"\nID=plexos\nVERSION_ID=0.1.0\n' > "${MOCK}/target/usr/lib/os-release"

# shellcheck source=post-image.sh disable=SC1091
source "${BOARD_DIR}/post-image.sh" "${MOCK}/images"
set +e   # post-image.sh sets -e; the checks below must be allowed to fail

WORK="${MOCK}/images/plexos-work"
IMAGE="${MOCK}/images/medialith.img"
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

    # And the half that pruning can get wrong. iwlwifi asks for revisions from its family's
    # UCODE_API_MAX downwards to that family's _MIN, so a blob outside that window is one
    # the card will never open -- and keeping only that one puts it back to having no
    # firmware, on an image whose firmware directory is visibly not empty. The code that
    # changes when this fails is install_wifi_firmware: keep the newest revision the kernel
    # still asks for, not the newest that exists.
    #
    # This was written against the highest MAX in the whole tree, which is a bound that
    # cannot see the failure worth catching. The AX210 family accepts 89 and the 8000s
    # accept up to 36, so an `iwlwifi-8265-37.ucode` appearing in linux-firmware would sit
    # far below the global maximum, pass, and be the only 8265 file shipped -- an 8265 with
    # no firmware, reported as fine.
    #
    # The mapping is derived, not written down. Each family declares its filename prefix as
    # <TOKEN>_FW_PRE, its window as <TOKEN>_UCODE_API_MAX/_MIN, and then ties the two
    # together itself:
    #
    #   MODULE_FIRMWARE(IWL3160_MODULE_FIRMWARE(IWL7260_UCODE_API_MAX));
    #
    # which is the kernel saying that the 3160's files are requested in the 7260's window --
    # a fact no table assembled by hand would have got right, and the same shape as the 3165
    # asking for 7265D files.
    if [ -z "${KCFG}" ]; then
        skipped "revisions are ones this kernel asks for" "no kernel source under ${OUTPUT}/build"
    else
        declare -A FW_PRE_OF=() PRE_FILE_OF=() API_MAX_OF=() API_MIN_OF=() \
                   FILE_APIS=() RANGE_OF=()

        # Which token declares which filename prefix, and in which file -- the file
        # matters, for the reason the intersection below explains.
        while IFS=: read -r file token value; do
            [ -n "${token}" ] || continue
            FW_PRE_OF["${token}"]="${value}"
            PRE_FILE_OF["${value}"]="${file}"
        done < <(grep -HE '^#define[[:space:]]+[A-Za-z0-9_]+_FW_PRE[[:space:]]+"' "${KCFG}"/*.c 2>/dev/null \
                 | sed -E 's|^([^:]+):#define[[:space:]]+([A-Za-z0-9_]+)_FW_PRE[[:space:]]+"([^"]+)".*|\1:\2:\3|')

        while IFS=: read -r file token value; do
            [ -n "${token}" ] || continue
            API_MAX_OF["${token}"]="${value}"
            FILE_APIS["${file}"]="${FILE_APIS[${file}]:-}${token} "
        done < <(grep -HE '^#define[[:space:]]+[A-Za-z0-9_]+_UCODE_API_MAX[[:space:]]+[0-9]+' "${KCFG}"/*.c 2>/dev/null \
                 | sed -E 's|^([^:]+):#define[[:space:]]+([A-Za-z0-9_]+)_UCODE_API_MAX[[:space:]]+([0-9]+).*|\1:\2:\3|')

        while IFS=: read -r file token value; do
            [ -n "${token}" ] && API_MIN_OF["${token}"]="${value}"
        done < <(grep -HE '^#define[[:space:]]+[A-Za-z0-9_]+_UCODE_API_MIN[[:space:]]+[0-9]+' "${KCFG}"/*.c 2>/dev/null \
                 | sed -E 's|^([^:]+):#define[[:space:]]+([A-Za-z0-9_]+)_UCODE_API_MIN[[:space:]]+([0-9]+).*|\1:\2:\3|')

        # The join, and how much of the window it is entitled to enforce.
        #
        # `iwl_get_ucode_api_versions()` builds the window a device actually asks for from
        # **two** declarations: the MAC family's and the RF module's, intersected, with
        # fallbacks when they do not overlap. Which of the two a firmware prefix is declared
        # beside decides how much this test can say about it, and the kernel's own file
        # layout is what separates them:
        #
        #   7000.c, 8000.c, 9000.c, 22000.c   declare a prefix and the family window it
        #                                     belongs to. Self-contained -- enforce both ends.
        #
        #   rf-hr.c, rf-jf.c, rf-gf.c         declare a prefix beside the *RF* window only.
        #                                     The MAC half is in a struct this cannot read.
        #
        # That second case is not academic. `IWL_QU_B_HR_B_FW_PRE` sits in rf-hr.c whose
        # window is 100..100, while a Qu's MAC family is 22000 at 77..77 -- so the device
        # asks for 77, which is exactly what linux-firmware ships. The first version of this
        # check enforced the RF window on its own and reported three perfectly good blobs as
        # sixteen revisions too old.
        #
        # What is still sound for those is the **upper** bound: the effective maximum is
        # min(MAC, RF), so it can never exceed the RF's. A blob above it is one no device
        # can request whatever its MAC says. The lower bound is not knowable here, and is
        # not guessed at.
        note_family() {
            local pre="$1" api="$2" file="$3"
            [ -n "${pre}" ] || return 0
            [ -n "${API_MAX_OF[${api}]:-}" ] || return 0
            case "$(basename "${file}")" in
                rf-*) RANGE_OF["${pre}"]="0 ${API_MAX_OF[${api}]} upper" ;;
                *)    RANGE_OF["${pre}"]="${API_MIN_OF[${api}]:-0} ${API_MAX_OF[${api}]} full" ;;
            esac
        }

        # `${call%_FW}` covers the families whose macro is spelled
        # <TOKEN>_FW_MODULE_FIRMWARE against a <TOKEN>_FW_PRE define.
        while IFS=: read -r file call api; do
            pre="${FW_PRE_OF[${call}]:-}"
            [ -n "${pre}" ] || pre="${FW_PRE_OF[${call%_FW}]:-}"
            note_family "${pre}" "${api}" "${file}"
        done < <(grep -HE '^MODULE_FIRMWARE\([A-Za-z0-9_]+_MODULE_FIRMWARE\([A-Za-z0-9_]+_UCODE_API_MAX\)\)' "${KCFG}"/*.c 2>/dev/null \
                 | sed -E 's|^([^:]+):MODULE_FIRMWARE\(([A-Za-z0-9_]+)_MODULE_FIRMWARE\(([A-Za-z0-9_]+)_UCODE_API_MAX\)\).*|\1:\2:\3|')

        # The AX210-era parts advertise firmware *and* their platform NVM through a second
        # macro, so a reader that knows only MODULE_FIRMWARE misses them entirely -- which
        # is how so-a0-gf-a0 and ty-a0-gf-a0 came to be checked against nothing but the
        # whole-tree maximum.
        while IFS=: read -r file token api; do
            note_family "${FW_PRE_OF[${token%_FW_PRE}]:-}" "${api}" "${file}"
        done < <(grep -HE '^IWL_FW_AND_PNVM\([A-Za-z0-9_]+,[[:space:]]*[A-Za-z0-9_]+_UCODE_API_MAX\)' "${KCFG}"/*.c 2>/dev/null \
                 | sed -E 's|^([^:]+):IWL_FW_AND_PNVM\(([A-Za-z0-9_]+),[[:space:]]*([A-Za-z0-9_]+)_UCODE_API_MAX\).*|\1:\2:\3|')

        assert "the kernel's own family-to-API mapping was derived (${#RANGE_OF[@]} families)" \
               "[ '${#RANGE_OF[@]}' -gt 10 ]" \
               "the parse found almost nothing, so every variant would fall through to the weak check and this stage would go quiet"

        # The parse has to still cover the families this image actually carries, or it
        # degrades to the old global bound without anybody noticing. Named ones, because a
        # count cannot tell which family stopped resolving.
        unmapped=""
        for want in iwlwifi-7260 iwlwifi-3160 iwlwifi-7265 iwlwifi-7265D iwlwifi-3168 \
                    iwlwifi-8000C iwlwifi-8265 iwlwifi-cc-a0 \
                    iwlwifi-9000-pu-b0-jf-b0 iwlwifi-9260-th-b0-jf-b0 \
                    iwlwifi-Qu-b0-hr-b0 iwlwifi-Qu-c0-hr-b0 iwlwifi-QuZ-a0-hr-b0 \
                    iwlwifi-Qu-b0-jf-b0 iwlwifi-Qu-c0-jf-b0 iwlwifi-QuZ-a0-jf-b0 \
                    iwlwifi-so-a0-jf-b0; do
            [ -n "${RANGE_OF[${want}]:-}" ] || unmapped="${unmapped} ${want}"
        done
        check "every family this image carries resolved to its own window" "${unmapped:-none}" "none"

        # A window for the variants the kernel names at runtime rather than declaring: the
        # AX210 and later parts build `iwlwifi-<mac>-<step>-<rf>-<step>` from hardware IDs
        # in iwl_drv_get_fwname_pre(), so no static prefix exists to join against. They fall
        # back to the whole-tree maximum, and the stage says which ones did rather than
        # letting a weaker check look like the strong one.
        global_max=$(grep -h 'UCODE_API_MAX' "${KCFG}"/*.c 2>/dev/null | grep -oE '[0-9]+$' | sort -n | tail -1)

        out_of_range=""
        fell_back=""
        upper_only=""
        for f in "${GOT}"/iwlwifi-*.ucode; do
            [ -e "${f}" ] || continue
            base=$(basename "${f}")
            rev="${base##*-}"; rev="${rev%.ucode}"
            case "${rev}" in ''|*[!0-9]*) continue ;; esac
            variant="${base%-*}"

            if [ -z "${RANGE_OF[${variant}]:-}" ]; then
                fell_back="${fell_back} ${variant}"
                [ "${rev}" -gt "${global_max:-0}" ] \
                    && out_of_range="${out_of_range} ${base}[above every API in the tree, ${global_max}]"
                continue
            fi

            read -r fmin fmax fkind <<< "${RANGE_OF[${variant}]}"
            if [ "${rev}" -gt "${fmax}" ]; then
                out_of_range="${out_of_range} ${base}[this family tops out at ${fmax}, too new]"
            elif [ "${fkind}" = "full" ] && [ "${fmin}" -gt 0 ] && [ "${rev}" -lt "${fmin}" ]; then
                out_of_range="${out_of_range} ${base}[this family asks ${fmin}..${fmax}, too old]"
            fi
            [ "${fkind}" = "upper" ] && upper_only="${upper_only} ${variant}"
        done

        check "every retained revision is inside its own family's window" \
              "${out_of_range:-none}" "none"

        # And the failure it exists for, exercised rather than assumed -- a check nobody has
        # seen fail is a check nobody knows works. This is the exact case the whole-tree
        # bound could not see: the 8000s top out at 36 while the tree's maximum is 100, so a
        # revision 37 appearing in linux-firmware would be kept as the newest 8265 file,
        # never requested by any 8265, and sail past a global comparison.
        read -r _ eight_max _ <<< "${RANGE_OF[iwlwifi-8265]:-0 0 full}"
        assert "a revision above the 8265's own maximum would be caught (${eight_max})" \
               "[ '${eight_max}' -gt 0 ] && [ 37 -gt '${eight_max}' ]" \
               "either the 8265 window stopped resolving, or this kernel moved and the example needs rechoosing"
        assert "while the one actually shipped is not" \
               "[ 36 -le '${eight_max}' ]"

        # What each variant was actually held to, because a check that covers two thirds of
        # a set and prints one green line reads as covering all of it.
        if [ -n "${upper_only}" ]; then
            printf '  %-6s %s\n' "info" \
                   "upper bound only, the MAC half of their window being in a struct rather than a define:$(printf '%s' "${upper_only}" | tr ' ' '\n' | sort -u | tr '\n' ' ')"
        fi
        if [ -n "${fell_back}" ]; then
            printf '  %-6s %s\n' "info" \
                   "checked against the whole-tree maximum only, the kernel naming these at probe time:$(printf '%s' "${fell_back}" | tr ' ' '\n' | sort -u | tr '\n' ' ')"
        fi
    fi

    # The families themselves. 6E is the one that is easy to leave out and the one an
    # Alder Lake machine needs; its blobs are named for the silicon rather than for the
    # product, so nothing about "AX211" appears anywhere in the filename.
    assert "the AX210 family is covered (AX210, AX211)" \
           "ls '${GOT}'/iwlwifi-*-gf-a0-*.ucode >/dev/null 2>&1" \
           "set BR2_PACKAGE_LINUX_FIRMWARE_IWLWIFI_6E; without it those laptops have no wlan0"
    assert "and the PNVM those parts need" "ls '${GOT}'/*.pnvm >/dev/null 2>&1" \
           "not a .ucode, so a glob written for ucode alone drops it and the card associates with nothing"

    # The older families, which are most of the second-hand and mini-PC hardware this gets
    # installed on. 7265D is the entry worth asserting by name: a 3165 does not ask for a
    # file called 3165 -- iwl3165_2ac_cfg sets .fw_name_pre = IWL7265D_FW_PRE -- so the
    # card is covered by this symbol and by nothing else, and the mapping is invisible in
    # both the filename and the Buildroot symbol.
    assert "the 7000 series is covered (7260, 7265)" \
           "ls '${GOT}'/iwlwifi-7260-*.ucode >/dev/null 2>&1 && ls '${GOT}'/iwlwifi-7265-*.ucode >/dev/null 2>&1" \
           "set BR2_PACKAGE_LINUX_FIRMWARE_IWLWIFI_7260 and _7265; a Haswell or Broadwell machine has no wlan0 without them"
    assert "and 7265D, which is what a 3165 actually asks for" \
           "ls '${GOT}'/iwlwifi-7265D-*.ucode >/dev/null 2>&1" \
           "set BR2_PACKAGE_LINUX_FIRMWARE_IWLWIFI_7265D; the 3165 is covered by this file and by nothing named 3165"
    assert "the 8000 series is covered (8260, 8265)" \
           "ls '${GOT}'/iwlwifi-8000C-*.ucode >/dev/null 2>&1 && ls '${GOT}'/iwlwifi-8265-*.ucode >/dev/null 2>&1" \
           "set BR2_PACKAGE_LINUX_FIRMWARE_IWLWIFI_8000C and _8265; the 8260 is 8000C, which is not a name anybody would guess"

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
check "it carries the MediaLith version rather than Buildroot's" \
      "$(sed -n 's/^VERSION_ID=//p' "${WORK}/os-release")" \
      "${PLEXOS_VERSION}"
assert "and the same file reaches the image tree" \
       "[ -s '${MOCK}/target/usr/lib/os-release' ]"
check "with the same version in it" \
      "$(sed -n 's/^VERSION_ID=//p' "${MOCK}/target/usr/lib/os-release")" \
      "${PLEXOS_VERSION}"

# The product name, which is what a person reads: the console header, the boot menu, and
# the vendor string Plex reports to its clients.
check "the product names itself MediaLith" \
      "$(sed -n 's/^NAME=//p' "${WORK}/os-release" | tr -d '\"')" \
      "MediaLith"
check "and says so with its version" \
      "$(sed -n 's/^PRETTY_NAME=//p' "${WORK}/os-release" | tr -d '\"')" \
      "MediaLith ${PLEXOS_VERSION}"

# And the two that did NOT change, asserted so that a later tidy-up cannot quietly take
# them. SORT_KEY is what systemd-boot groups entries by: an ESP holding one UKI keyed
# `plexos` and another keyed `medialith` is two groups, and how the bootloader orders
# between groups is not established here. ID has no consumer at all, so changing it buys
# nothing and risks the same surprise.
check "the boot sort key is still the legacy one" \
      "$(sed -n 's/^SORT_KEY=//p' "${WORK}/os-release")" \
      "plexos"
check "and so is the os-release ID" \
      "$(sed -n 's/^ID=//p' "${WORK}/os-release")" \
      "plexos"

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

# --------------------------------------------------------------------------
stage "stage 7 — every shipped kernel module is one something loads"
#
# CONFIG_MODULES=y exists for exactly one reason: NVIDIA's open kernel modules are
# out-of-tree and cannot be built in. Everything else in this image is `=y`, and there is
# no udev, no kmod and no modprobe -- so a `.ko` that arrives for any other reason is a
# feature that is silently gone. It compiles, it installs, it passes every test, and the
# thing it does never happens on a machine.
#
# That is not hypothetical. Turning MODULES on handed kconfig a third answer for every
# tristate symbol and it took it eleven times: `efivarfs` was caught and pinned, and the
# other eight modules shipped for a further release. One of them was
# `x86_pkg_temp_thermal`, which publishes the only thermal zone that reports the processor
# die -- so the activity card fell back to a chassis sensor and reported a real temperature
# of the wrong thing, with nothing failing and nothing logged.
#
# The allow-list is not a list. It is `plexos_init::nvidia::MODULES` -- the names PID 1
# actually passes to `finit_module` -- read out of the source. A hand-kept list would be a
# second place to update and therefore a place to forget; taking it from the loader makes
# "shipped but never loaded" impossible to express rather than merely tested for.
NVIDIA_RS="${BOARD_DIR}/../../../../crates/plexos-init/src/nvidia.rs"
MODULES_DIR="${OUTPUT}/target/usr/lib/modules"

# Is the driver that needs modules actually in this build? Read from the *effective*
# Buildroot config where there is one, because that is what the build used; the defconfig
# is the fallback for a tree that has not been configured yet.
BR_CONFIG="${OUTPUT}/.config"
[ -r "${BR_CONFIG}" ] || BR_CONFIG="${BOARD_DIR}/../../../configs/plexos_x86_64_defconfig"
NVIDIA_WANTED=0
grep -q '^BR2_PACKAGE_PLEXOS_NVIDIA=y' "${BR_CONFIG}" 2>/dev/null && NVIDIA_WANTED=1

# The audit itself, as a function, for one reason: the interesting case is a modules
# directory that is not there at all, and the only honest way to test that is to call this
# with a path that does not exist. Leaving it inline meant the absent case could only be
# reached by breaking a real build, so it was never exercised -- and what it did was skip
# the whole stage, silently, including on a build where NVIDIA is enabled and every
# expected module is therefore missing. A skip reads as a pass.
#
#   $1  the modules directory to audit
#   $2  1 if BR2_PACKAGE_PLEXOS_NVIDIA is enabled for this build
#
# Prints one line per problem and returns non-zero if there were any.
module_audit() {
    local dir="$1" nvidia="$2" problems="" module
    local loaded shipped

    loaded=$(awk '/pub const MODULES/,/;/' "${NVIDIA_RS}" \
             | grep -oE '"[a-z0-9_-]+"' | tr -d '"' | sort -u)

    if [ ! -d "${dir}" ]; then
        # No directory is the correct and expected state for an Intel-only image: nothing
        # is out of tree, so nothing needs a loader. It is a defect only when something is
        # supposed to be in there.
        if [ "${nvidia}" = "1" ]; then
            printf 'no modules directory at %s, but BR2_PACKAGE_PLEXOS_NVIDIA is enabled: %s\n' \
                   "${dir}" "$(printf '%s' "${loaded}" | tr '\n' ' ')is what plexos-init will try to load"
            return 1
        fi
        return 0
    fi

    shipped=$(find "${dir}" -name '*.ko' -printf '%f\n' 2>/dev/null | sed 's/\.ko$//' | sort -u)

    # Every shipped module must be one the loader names.
    for module in ${shipped}; do
        printf '%s\n' "${loaded}" | grep -qx "${module}" \
            || problems="${problems}ships but nothing loads it: ${module}"$'\n'
    done

    # And the other direction, which only matters when the package that provides them is in.
    if [ "${nvidia}" = "1" ]; then
        for module in ${loaded}; do
            printf '%s\n' "${shipped}" | grep -qx "${module}" \
                || problems="${problems}the loader names it and it is not shipped: ${module}"$'\n'
        done
    fi

    [ -z "${problems}" ] && return 0
    printf '%s' "${problems}"
    return 1
}

if [ ! -r "${NVIDIA_RS}" ]; then
    skipped "the whole stage" "needs ${NVIDIA_RS} to read the loader's own module list"
else
    # The names between the brackets of `pub const MODULES: [&str; N] = [...]`, one per
    # line. Parsed rather than duplicated, for the reason in the comment above.
    #
    # awk's range and not sed's: sed looks for the closing pattern from the *next* line, so
    # a declaration that opens and closes on one line runs on to the next `];` in the file
    # and sweeps up every quoted string in between. That is not a hypothetical either --
    # this stage was written with sed and reported ten modules missing, with names like
    # `blkext` and `vendor` taken from unrelated code further down.
    LOADED=$(awk '/pub const MODULES/,/;/' "${NVIDIA_RS}" \
             | grep -oE '"[a-z0-9_-]+"' | tr -d '"' | sort -u)
    assert "the loader's module list can be read" "[ -n '${LOADED}' ]" \
           "pub const MODULES in nvidia.rs did not parse; this stage cannot check anything without it"

    # And that it was read *whole*. The declaration states its own length, so comparing the
    # count against it turns a parser that quietly matched too much or too little into a
    # failed build -- which is the mistake this stage has already made once.
    DECLARED=$(grep -oE 'pub const MODULES: \[&str; [0-9]+\]' "${NVIDIA_RS}" \
               | grep -oE '[0-9]+')
    check "the list is read whole, against the length it declares" \
          "$(printf '%s\n' "${LOADED}" | grep -c .)" "${DECLARED:-?}"

    printf '  %-6s %s\n' "info" \
           "BR2_PACKAGE_PLEXOS_NVIDIA is $([ "${NVIDIA_WANTED}" = 1 ] && echo enabled || echo 'not set'), per ${BR_CONFIG##*/}"

    # The real audit. Its output is the list of problems, so the failure names them.
    audit_problems="$(module_audit "${MODULES_DIR}" "${NVIDIA_WANTED}")" && audit_ok=1 || audit_ok=0
    if [ "${audit_ok}" = "1" ]; then
        ok "every shipped module is one the loader names, and nothing is missing"
    else
        bad "the shipped module set does not match the loader's" \
            "$(printf '%s' "${audit_problems}" | tr '\n' ';') -- pin an unexpected module's CONFIG_* to =y in linux.fragment (or =n if MediaLith does not want it); a missing one means the plexos-nvidia package did not build"
    fi

    # The edge case this stage used to get wrong, exercised rather than reasoned about.
    #
    # A build with NVIDIA enabled and no modules directory at all is the worst version of
    # this failure: plexos-init calls finit_module on four paths that do not exist, an RTX
    # machine comes up with no NVDEC, and the old code answered by skipping the stage --
    # which prints "skip" and counts as not-failed. Both directions are checked, because a
    # rule that fails an Intel-only image would be its own defect.
    absent="${TMP}/no-such-modules-dir"
    rm -rf "${absent}"
    assert "a missing modules directory FAILS when NVIDIA is enabled" \
           "! module_audit '${absent}' 1 >/dev/null" \
           "this is the case the stage used to skip; a skip reads as a pass"
    assert "and is accepted when NVIDIA is not" \
           "module_audit '${absent}' 0 >/dev/null" \
           "an Intel-only image ships no modules at all and that is correct, not a fault"

    # Named on its own because it is the one that was actually wrong, and because a
    # regression here is invisible: the page keeps showing a temperature.
    assert "the processor die thermal driver is built in, not shipped as a module" \
           "! find '${MODULES_DIR}' -name 'x86_pkg_temp_thermal.ko' 2>/dev/null | grep -q ." \
           "CONFIG_X86_PKG_TEMP_THERMAL went back to =m: nothing loads it, the x86_pkg_temp zone never appears, and metrics falls back to acpitz -- a chassis sensor reported as the processor"
fi

# --------------------------------------------------------------------------
stage "stage 7b — the kernel config contract"
#
# Read from the *effective* .config the kernel was built with, not from
# linux.fragment. A fragment states a request; kconfig decides, and it drops an option
# whose dependency is unmet **without erroring** -- the trap that cost this project four
# months of an unreadable console, and again the reason three USB Ethernet drivers were
# absent from .config without ever appearing as refused.
#
# Only options whose absence loses something a person would notice, each with what it
# loses. A list of every symbol MediaLith sets would be linux.fragment written twice, and
# the second copy is the one that goes stale.
KCONFIG="$(ls -d "${OUTPUT}"/build/linux-*/.config 2>/dev/null | head -1)"
if [ -z "${KCONFIG}" ] || [ ! -r "${KCONFIG}" ]; then
    skipped "the whole stage" "no built kernel .config under ${OUTPUT}/build/linux-*"
else
    builtin_or_bad() {
        local symbol="$1" loses="$2" got
        got=$(grep -E "^(CONFIG_${symbol}=|# CONFIG_${symbol} is not set)" "${KCONFIG}" \
              || printf '(absent from .config)')
        if [ "${got}" = "CONFIG_${symbol}=y" ]; then
            ok "CONFIG_${symbol}=y"
        else
            bad "CONFIG_${symbol}=y" "got '${got}' -- ${loses}"
        fi
    }

    # Storage. Anything on this path that is not built in is a kernel that cannot reach
    # its own root, and there is no initramfs module to rescue it.
    builtin_or_bad BLK_DEV_NVME "no NVMe disk at all"
    builtin_or_bad SATA_AHCI    "no SATA disk at all"
    builtin_or_bad VMD          "NVMe behind Intel VMD/RST disappears entirely: no slow disk and no degraded disk, no disk, on a machine whose firmware lists the drive by model"

    # Network. The appliance brings links up during boot; a driver that is not there when
    # plexosd looks is an appliance with no address and a page nobody can reach.
    builtin_or_bad USB_RTL8152           "no Realtek USB Ethernet -- the reference laptop's only wired link"
    builtin_or_bad USB_USBNET            "the framework the three below sit on; without it they vanish from .config entirely rather than being refused"
    builtin_or_bad USB_NET_AX88179_178A  "no ASIX USB Ethernet, which is most USB 3 gigabit adapters sold"
    builtin_or_bad USB_NET_CDCETHER      "no CDC Ethernet, which is what docks and tethered phones speak"
    builtin_or_bad USB_NET_CDC_NCM       "no CDC NCM, the successor most current CDC devices actually use"

    # Virtual machines. A virtio disk that became a module is a guest that does not boot.
    builtin_or_bad VIRTIO_PCI  "no virtio devices are discovered at all"
    builtin_or_bad VIRTIO_BLK  "no virtio-blk disk -- the default in plain QEMU invocations"
    builtin_or_bad SCSI_VIRTIO "no virtio-scsi disk -- the default in Proxmox"
    builtin_or_bad VIRTIO_NET  "no network in a VM"

    # Features whose absence is silent, which is what makes them worth asserting.
    builtin_or_bad CIFS "plexosd::shares mounts smb3 and the kernel has never heard of it, so every SMB mount fails with ENODEV on an appliance whose console offers an SMB form"
    builtin_or_bad X86_PKG_TEMP_THERMAL "no x86_pkg_temp zone, so metrics falls back to acpitz -- a chassis sensor reported as the processor die, with nothing failing and nothing logged"
    builtin_or_bad EFIVAR_FS "PID 1 cannot read LoaderDevicePartUUID, so a machine with two MediaLith disks goes back to resolving partitions by label, which is a coin toss"

    # The filesystems a library actually arrives on. Each of these is a tristate, and
    # CONFIG_MODULES is on -- so kconfig has a third answer available for every one of
    # them, and =m here is a filesystem that is silently gone in an image with no
    # modprobe. That is the eleven-options-became-=m trap, pointed at the feature a
    # person notices immediately.
    builtin_or_bad NTFS3_FS "the Windows partition on the internal disk cannot be opened at all, which is where the library is on any machine MediaLith is booted from a stick beside"
    builtin_or_bad EXFAT_FS "no exFAT, which is how Windows formats every USB drive above 32 GB"
    builtin_or_bad EXT4_FS  "no ext4, so a drive formatted on a Linux machine cannot be read"
    builtin_or_bad MSDOS_PARTITION "a USB drive with an MBR partition table enumerates as a whole disk with no partitions on it -- so the library is invisible on a disk the machine can see. It is default y and nothing in linux.fragment asks for it, which is exactly why it is asserted here rather than assumed"

    # And the wireless driver, which is why the firmware stage above has anything to do.
    builtin_or_bad IWLWIFI "no Intel wireless"
    builtin_or_bad IWLMVM  "no Intel wireless on anything from the 7000 series onwards, which is all of it here"
fi

stage "stage 8 — the CPU baseline contract"
#
# MediaLith targets generic x86-64. That is a product decision, and it is one that is
# very easy to undo by accident: `BR2_x86_corei7` sat in the defconfig for the whole
# life of the project without anybody choosing it, and what it bought was a userspace
# that dies of SIGILL on any processor below Nehalem — *after* the kernel has booted
# and after PID 1 has run, so the machine looks like it got much further than it did.
#
# Nothing in the image needs anything above the baseline: the workspace's own binaries
# are built for x86_64-unknown-linux-gnu with no -C target-cpu, and Plex carries its
# own musl runtime and dispatches on CPUID. So the floor can only ever come back by
# mistake, and this is where the mistake is caught — at the configuration decision,
# not on somebody's Core 2.
BRCONFIG="${OUTPUT}/.config"
if [ ! -r "${BRCONFIG}" ]; then
    skipped "the effective Buildroot config" "no .config at ${BRCONFIG}"
else
    check "BR2_GCC_TARGET_ARCH is generic x86-64" \
          "$(grep -E '^BR2_GCC_TARGET_ARCH=' "${BRCONFIG}" || echo '(absent)')" \
          'BR2_GCC_TARGET_ARCH="x86-64"'
    assert "BR2_x86_x86_64=y is the selected variant" \
           "grep -qx 'BR2_x86_x86_64=y' '${BRCONFIG}'" \
           "the generic x86-64 architecture variant is not selected"

    # Every variant above the baseline, by the feature symbols they select rather than
    # by name. A list of CPU names would need extending every time Buildroot adds a
    # part; these four are what the baseline is *defined* by not having, so a variant
    # nobody has heard of yet still trips this.
    for sym in SSE3 SSSE3 SSE4 SSE42 AVX AVX2 AVX512; do
        assert "BR2_X86_CPU_HAS_${sym} is not set" \
               "! grep -qx 'BR2_X86_CPU_HAS_${sym}=y' '${BRCONFIG}'" \
               "something selected a CPU variant above the x86-64 baseline; \
grep '^BR2_x86_.*=y' ${BRCONFIG} to see which"
    done
fi

# And what the compiler was actually configured with, which is the half the defconfig
# cannot promise. BR2_GCC_TARGET_ARCH reaches gcc as --with-arch=, baked in as the
# default -march for every target compilation, so this is the artefact rather than the
# request — the same distinction stage 7b draws for the kernel.
CROSS_GCC="$(ls "${BR_HOST}"/bin/*-linux-gnu*-gcc 2>/dev/null | head -1)"
if [ -z "${CROSS_GCC}" ] || [ ! -x "${CROSS_GCC}" ]; then
    skipped "the cross compiler's own --with-arch" "no cross gcc under ${BR_HOST}/bin"
else
    check "the cross compiler defaults to -march=x86-64" \
          "$("${CROSS_GCC}" -v 2>&1 | grep -o -- '--with-arch=[^ ]*' | head -1)" \
          "--with-arch=x86-64"
fi

# And the artefacts themselves — by running them, not by disassembling them.
#
# The obvious check is to grep a binary for instructions the baseline does not have.
# **That check is unsound, and it was written here and failed on its first honest
# test.** Scanning the generic busybox finds `pshufb`, `palignr` and `sha256rnds2`
# after the toolchain was moved to -march=x86-64 — because busybox ships hand-written
# SHA-1 and SHA-256 assembly in libbb/hash_sha*_hwaccel_x86-64.S and reaches it only
# through `get_shaNI()`, which asks CPUID first. glibc does the same thing through
# IFUNC. So the presence of a post-baseline instruction says nothing at all about the
# floor: what matters is whether anything *executes* it on a processor that lacks it,
# and only running the binary can answer that.
#
# qemu-user is what turns that into a test. It decodes every instruction against the
# named model, so a binary that reaches its exit on an Opteron_G1 model has taken a
# path containing nothing above the x86-64 baseline. Not a proof about every path —
# a program has many — but a direct reading of the one that matters, which is startup.
QEMU_USER="$(command -v qemu-x86_64-static || command -v qemu-x86_64 || true)"
[ -z "${QEMU_USER}" ] && [ -x "${PLEXOS_QEMU_USER:-}" ] && QEMU_USER="${PLEXOS_QEMU_USER}"
LOADER="${OUTPUT}/target/lib/ld-linux-x86-64.so.2"
if [ -z "${QEMU_USER}" ]; then
    skipped "target binaries start on a baseline CPU" \
            "no qemu-x86_64 on this host; install qemu-user-static or set PLEXOS_QEMU_USER"
elif [ ! -x "${LOADER}" ]; then
    skipped "target binaries start on a baseline CPU" "no ${LOADER}"
else
    for entry in "busybox:bin/busybox:--help" \
                 "curl:usr/bin/curl:--version" \
                 "gpgv:usr/bin/gpgv:--version" \
                 "veritysetup:usr/sbin/veritysetup:--version" \
                 "wpa_supplicant:usr/sbin/wpa_supplicant:-v"; do
        name="${entry%%:*}"; rest="${entry#*:}"
        rel="${rest%%:*}"; args="${rest#*:}"
        bin="${OUTPUT}/target/${rel}"
        if [ ! -x "${bin}" ]; then
            skipped "${name} starts on an Opteron_G1 CPU model" "no ${bin}"
            continue
        fi
        # Opteron_G1 is the oldest AMD64 model QEMU offers: measured here to have no
        # SSSE3, no SSE4, no POPCNT and no CMPXCHG16B. If it runs there it runs on
        # anything that can execute the 64-bit ABI at all.
        out="$( { timeout 90 "${QEMU_USER}" -cpu Opteron_G1 "${LOADER}" \
                    --library-path "${OUTPUT}/target/lib:${OUTPUT}/target/usr/lib" \
                    "${bin}" "${args}"; } 2>&1 )"
        rc=$?
        if [ ${rc} -eq 132 ] || grep -qi "illegal instruction" <<<"${out}"; then
            bad "${name} starts on an Opteron_G1 CPU model" \
                "SIGILL: something in the startup path is above the x86-64 baseline"
        elif [ ${rc} -eq 124 ]; then
            bad "${name} starts on an Opteron_G1 CPU model" "timed out"
        else
            ok "${name} starts on an Opteron_G1 CPU model"
        fi
    done
fi

stage "stage 9 — no package sync can recurse into an output tree"
#
# The three plexos-* packages are built with OVERRIDE_SRCDIR pointing at this
# repository, so Buildroot rsyncs the whole tree into `output*/build/<pkg>/`. The
# destination is therefore *inside* the source, and any root-level build tree the
# exclude list forgets is copied into itself.
#
# That failure has no error and no bad exit status. It is a recursion: rsync keeps
# finding more to copy, the build appears to sit on "Syncing from source dir", and the
# disk fills. It happened here — `--exclude=output` covered the default tree and not the
# `output-corei7` and `output-generic` trees the CPU-baseline work added, and one sync
# took the filesystem from 20 GiB to 698 GiB before anybody looked at `df`.
#
# So this tests the property rather than the spelling. It pulls each package's real
# exclusion list out of its .mk, runs an actual rsync with it over a tree containing all
# the traps, and asks what arrived. A different but equivalent set of patterns passes; a
# clever rewrite that stops excluding something does not.
#
# It also checks the other half, which is the one a blunt fix breaks: a *nested*
# directory that merely has "output" in its name must still be copied, because excluding
# it would silently ship a package missing a directory nobody thought to look for.
if ! command -v rsync >/dev/null 2>&1; then
    skipped "the whole stage" "no rsync on this host"
else
    SYNCSRC="${TMP}/syncsrc"
    mkdir -p "${SYNCSRC}"/{output,output-corei7,output-generic,target,.git} \
             "${SYNCSRC}/crates/plexos-init/src" \
             "${SYNCSRC}/docs/output"
    # One file in each, so an empty directory cannot be mistaken for an excluded one.
    for d in output output-corei7 output-generic target .git crates/plexos-init/src \
             docs/output; do
        echo marker > "${SYNCSRC}/${d}/marker"
    done
    echo marker > "${SYNCSRC}/Cargo.toml"

    for pkg in plexos-init plexosd plexos-gpu; do
        mk="${BOARD_DIR}/../../../package/${pkg}/${pkg}.mk"
        if [ ! -r "${mk}" ]; then
            bad "${pkg}: exclusions" "no ${mk}"
            continue
        fi
        # Every --exclude= token in the file. Reading the whole file rather than one
        # variable keeps this working if the list is ever split or renamed.
        mapfile -t excludes < <(grep -oE -- '--exclude=[^ \\]+' "${mk}")
        if [ "${#excludes[@]}" -eq 0 ]; then
            bad "${pkg}: exclusions" "no --exclude= patterns found in ${mk}"
            continue
        fi

        dest="${TMP}/syncdest-${pkg}"
        rm -rf "${dest}"; mkdir -p "${dest}"
        rsync -a "${excludes[@]}" "${SYNCSRC}/" "${dest}" >/dev/null 2>&1

        # The invariant. Each of these is a directory that must not have been copied.
        for forbidden in output output-corei7 output-generic target .git; do
            assert "${pkg}: does not copy /${forbidden}" \
                   "[ ! -e '${dest}/${forbidden}' ]" \
                   "a root-level ${forbidden} reached the package build directory; \
with OVERRIDE_SRCDIR that is a recursion, not a wasted copy"
        done
        # And the two that must have been.
        assert "${pkg}: still copies nested docs/output" \
               "[ -e '${dest}/docs/output/marker' ]" \
               "an unanchored exclude removed a nested directory that only shares the name"
        assert "${pkg}: still copies the sources" \
               "[ -e '${dest}/crates/plexos-init/src/marker' ] && [ -e '${dest}/Cargo.toml' ]" \
               "the exclusion list removed something the package needs to build"
    done
fi

printf '\npassed %d, failed %d, skipped %d\n' "${pass}" "${fail}" "${skip}"
[ "${fail}" -eq 0 ]
