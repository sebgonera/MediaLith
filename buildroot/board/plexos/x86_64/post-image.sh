#!/usr/bin/env bash
#
# Assemble a bootable PlexOS disk image.
#
# NOT YET RUN END TO END, and no image produced by this script has ever been booted.
# What has actually been executed, against real tools on a mock target tree:
#
#   stage 1  /usr erofs        yes — including a byte-identical rebuild
#   stage 2  verity tree       yes — root hash parsed, verified, and shown to fail
#                                    on a tampered image
#   stage 3  initrd            no  — package/plexos-init/ does not exist yet
#   stage 4  UKI               no  — needs linuxx64.efi.stub from the build
#   stage 5  ESP               no  — needs host-mtools from the build
#   stage 6  GPT and placement yes — all six partitions, types, and offsets checked
#
# Update this table when a stage is genuinely exercised, and not before.
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

PLEXOS_VERSION="${PLEXOS_VERSION:-0.1.0}"

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
# 1. The /usr image
# --------------------------------------------------------------------------
# Only /usr, not the whole target tree. /usr *is* the unit of update (ADR-0001), and
# the root is a tmpfs assembled at boot, so anything outside /usr in the target
# directory is either recreated by plexos-init or deliberately discarded.
build_usr_image() {
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

    ( cd "${WORK}/initrd" && find . -print0 \
        | sort -z \
        | cpio --null --create --format=newc --quiet --reproducible ) \
        > "${WORK}/initrd.cpio"

    msg "  initrd is $(( $(stat -c %s "${WORK}/initrd.cpio") / 1024 )) KiB"
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
    msg "assembling Unified Kernel Image"

    # The root hash rides on the command line, inside the signed artifact. This single
    # line is what makes the UKI signature transitively cover every byte of /usr
    # (ADR-0004), and it is why this stage cannot run before the verity stage.
    printf 'plexos.slot=a plexos.roothash=%s console=tty0 console=ttyS0,115200\n' \
        "${ROOT_HASH}" > "${WORK}/cmdline"

    if [ -f "${TARGET_DIR}/usr/lib/os-release" ]; then
        cp "${TARGET_DIR}/usr/lib/os-release" "${WORK}/os-release"
    else
        printf 'NAME="PlexOS"\nID=plexos\nVERSION_ID=%s\nPRETTY_NAME="PlexOS %s"\n' \
            "${PLEXOS_VERSION}" "${PLEXOS_VERSION}" > "${WORK}/os-release"
    fi

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

    objcopy "${args[@]}" "${stub}" "${WORK}/plexos.efi"

    # Verify the sections actually landed. objcopy exits 0 when asked to add a section
    # to a PE file with no room reserved for it, and the result is a binary that the
    # firmware loads and the stub then cannot find its kernel in.
    for section in .osrel .cmdline .linux .initrd; do
        objdump -h "${WORK}/plexos.efi" | grep -q " ${section}\$\| ${section} " || die \
            "section ${section} is missing from the assembled UKI" \
            "the stub may lack reserved section headers; check -Defi-stub-extra-sections in package/plexos-systemd-boot"
    done

    if [ -n "${PLEXOS_SB_KEY}" ] && [ -n "${PLEXOS_SB_CERT}" ]; then
        command -v sbsign >/dev/null 2>&1 || die \
            "PLEXOS_SB_KEY is set but sbsign is not installed" \
            "apt install sbsigntool, or unset PLEXOS_SB_KEY to build an unsigned image"
        msg "  signing UKI"
        sbsign --key "${PLEXOS_SB_KEY}" --cert "${PLEXOS_SB_CERT}" \
               --output "${WORK}/plexos-signed.efi" "${WORK}/plexos.efi"
        mv "${WORK}/plexos-signed.efi" "${WORK}/plexos.efi"
    else
        msg "  UNSIGNED (set PLEXOS_SB_KEY and PLEXOS_SB_CERT to sign)"
        msg "  Secure Boot must be turned off in firmware to boot this image"
    fi

    msg "  UKI is $(( $(stat -c %s "${WORK}/plexos.efi") / 1024 / 1024 )) MiB"
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
    "${mcopy}" -i "${esp}" "${BINARIES_DIR}/systemd-bootx64.efi" ::/EFI/BOOT/BOOTX64.EFI
    "${mcopy}" -i "${esp}" "${BINARIES_DIR}/systemd-bootx64.efi" ::/EFI/systemd/systemd-bootx64.efi

    # The try counter lives in the filename (ADR-0005): "+3" means three attempts
    # remain and none has been used. systemd-boot decrements it by renaming before
    # handing off, and plexosd drops the suffix entirely once the health gate passes.
    "${mcopy}" -i "${esp}" "${WORK}/plexos.efi" "::/EFI/Linux/plexos-${PLEXOS_VERSION}+3.efi"

    printf 'timeout 3\ndefault plexos-*\n' > "${WORK}/loader.conf"
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
    local -a sgdisk_args=()
    if [ -x "${layout_bin}" ]; then
        mapfile -t sgdisk_args < <("${layout_bin}" --format sgdisk)
    elif command -v cargo >/dev/null 2>&1 && [ -f "${REPO_ROOT}/Cargo.toml" ]; then
        # Bring-up path: no host package for the emitter yet, but the workspace is
        # right there. Explicit rather than silent, because it makes the image build
        # depend on a toolchain Buildroot does not manage.
        msg "  (building plexos-layout from the workspace; no host package yet)"
        mapfile -t sgdisk_args < <(
            cargo run --quiet --manifest-path "${REPO_ROOT}/Cargo.toml" \
                  -p plexos-types --bin plexos-layout -- --format sgdisk
        )
    else
        die "cannot determine the partition layout" \
            "build plexos-layout into \$HOST_DIR/bin, or make cargo available so it can be built from ${REPO_ROOT}"
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

main() {
    preflight
    rm -rf "${WORK}"
    mkdir -p "${WORK}"

    build_usr_image
    build_verity
    build_initrd
    build_uki
    build_esp
    build_disk

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
