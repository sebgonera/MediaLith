#!/bin/sh
# Capture everything PlexOS needs to know about a candidate machine.
#
# Read-only: this script inspects and prints. It writes nothing, installs nothing,
# and loads no modules. Safe to run on a machine you care about.
#
# Runs on any Linux system with a POSIX shell. No Rust, no dependencies beyond
# coreutils; anything else it uses is optional and its absence is itself recorded.
#
#     sh tools/capture-hardware.sh > capture.txt
#
# Some sections need root and say so when they cannot be read. Run under sudo for
# the complete picture, but an unprivileged capture is still useful.

set -u

section() {
    printf '\n===== %s =====\n' "$1"
}

# Print a file's contents, or say why not. Never fails the script.
show() {
    if [ -r "$1" ]; then
        cat "$1" 2>/dev/null || echo "(unreadable: $1)"
    elif [ -e "$1" ]; then
        echo "(exists but not readable without root: $1)"
    else
        echo "(absent: $1)"
    fi
}

have() {
    command -v "$1" >/dev/null 2>&1
}

echo "PlexOS hardware capture"
echo "generated: $(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo unknown)"
echo "user: $(id -un 2>/dev/null || echo unknown) (uid $(id -u 2>/dev/null || echo '?'))"

section "CPU"
if [ -r /proc/cpuinfo ]; then
    grep -m1 'model name' /proc/cpuinfo 2>/dev/null || echo "(no model name line)"
    printf 'cores: %s\n' "$(grep -c '^processor' /proc/cpuinfo 2>/dev/null || echo '?')"
else
    echo "(no /proc/cpuinfo)"
fi
printf 'arch: %s\n' "$(uname -m)"

section "Kernel"
uname -srv

section "Firmware / UEFI"
if [ -d /sys/firmware/efi ]; then
    echo "UEFI: yes"
    printf 'efi bitness: '
    show /sys/firmware/efi/fw_platform_size
else
    echo "UEFI: NO — this machine booted in legacy BIOS mode."
    echo "PlexOS requires UEFI (ADR-0003). Check the firmware setup for a CSM or"
    echo "Legacy Boot option and disable it, then re-run this capture."
fi

printf 'secure boot: '
sb=$(find /sys/firmware/efi/efivars -name 'SecureBoot-*' 2>/dev/null | head -n1)
if [ -n "$sb" ] && [ -r "$sb" ]; then
    # 5-byte variable: 4 bytes of attributes, then the flag.
    val=$(od -An -t u1 "$sb" 2>/dev/null | tr -s ' ' | sed 's/^ //' | cut -d' ' -f5)
    case "$val" in
        1) echo "enabled" ;;
        0) echo "disabled" ;;
        *) echo "unknown (raw: ${val:-none})" ;;
    esac
elif have mokutil; then
    mokutil --sb-state 2>&1 || echo "unknown"
else
    echo "could not determine (needs root, or efivars not mounted)"
fi

section "Graphics devices (lspci)"
if have lspci; then
    lspci -nn 2>/dev/null | grep -Ei 'vga|display|3d' || echo "(no display controllers listed)"
else
    echo "(lspci not installed — the DRM section below covers the same ground)"
fi

section "DRM nodes"
# This is the exact data plexos-gpu reads. A capture here can be turned straight
# into a test fixture.
if [ -d /sys/class/drm ]; then
    for node in /sys/class/drm/*; do
        name=$(basename "$node")
        case "$name" in
            card*|renderD*) ;;
            *) continue ;;
        esac
        printf -- '--- %s\n' "$name"
        printf '  vendor: '; show "$node/device/vendor" | tr -d '\n'; echo
        printf '  device: '; show "$node/device/device" | tr -d '\n'; echo
        if [ -L "$node/device/driver" ]; then
            printf '  driver: %s\n' "$(basename "$(readlink "$node/device/driver")")"
        else
            echo "  driver: (none bound)"
        fi
    done
else
    echo "(no /sys/class/drm — no DRM subsystem)"
fi

section "Render node permissions"
# Plex runs unprivileged, so group ownership here decides whether it can transcode.
ls -l /dev/dri 2>/dev/null || echo "(no /dev/dri)"

section "Intel GuC/HuC firmware"
# Needs debugfs mounted and root. Unknown here is normal and not a problem.
found=0
for p in /sys/kernel/debug/dri/0/gt0/uc/guc_info \
         /sys/kernel/debug/dri/0/gt/uc/guc_info \
         /sys/kernel/debug/dri/0/i915_guc_load_status \
         /sys/kernel/debug/dri/0/gt0/uc/huc_info \
         /sys/kernel/debug/dri/0/gt/uc/huc_info \
         /sys/kernel/debug/dri/0/i915_huc_load_status; do
    if [ -e "$p" ]; then
        found=1
        printf -- '--- %s\n' "$p"
        show "$p"
    fi
done
[ "$found" -eq 0 ] && echo "(no GuC/HuC debugfs entries; needs root and a mounted debugfs)"

section "i915 firmware messages"
if have dmesg; then
    # Exit status of a pipeline is the last command's, so testing it here would
    # always see tail's success. Capture, then check for emptiness.
    msgs=$(dmesg 2>/dev/null | grep -Ei 'i915|xe |huc|guc|firmware' | tail -n 30)
    if [ -n "$msgs" ]; then
        echo "$msgs"
    else
        echo "(nothing matched, or dmesg needs root)"
    fi
else
    echo "(dmesg not available)"
fi

section "VA-API (vainfo)"
if have vainfo; then
    vainfo 2>&1
else
    echo "(vainfo not installed)"
    echo "This is the single most useful thing in the capture. Install it:"
    echo "  Debian/Ubuntu:  sudo apt install vainfo intel-media-va-driver"
    echo "  Fedora:         sudo dnf install libva-utils intel-media-driver"
    echo "  Arch:           sudo pacman -S libva-utils intel-media-driver"
fi

section "Block devices"
# Relevant to the installer: PlexOS needs a whole disk (ADR-0003).
if have lsblk; then
    lsblk -o NAME,SIZE,TYPE,TRAN,MODEL 2>/dev/null || lsblk 2>/dev/null
else
    echo "(lsblk not installed)"
    show /proc/partitions
fi

section "Memory"
grep -E '^Mem(Total|Available)' /proc/meminfo 2>/dev/null || echo "(no /proc/meminfo)"

printf '\n===== end of capture =====\n'
