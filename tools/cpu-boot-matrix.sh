#!/usr/bin/env bash
#
# Boots the actual MediaLith image under QEMU + OVMF on a named CPU model and reports
# how far it got.
#
#   tools/cpu-boot-matrix.sh <image> <cpu-model> [seconds]
#
# # Why TCG and not KVM, on a host that has KVM
#
# KVM cannot test a CPU baseline. `-cpu Conroe` under KVM changes what CPUID *reports*;
# it does not change what the silicon underneath will execute. The host here is an
# i7-14700, so an SSE4.2 instruction in a Buildroot binary runs perfectly on a guest
# that claims to be a Core 2 — and the matrix comes back all-green about a floor that
# is still there. Only TCG actually decodes each instruction against the model, and
# only TCG therefore raises SIGILL where a real Core 2 would.
#
# So every historical model here runs under `accel=tcg`, deliberately and at the cost of
# several minutes per boot. `-cpu host` under KVM is a different measurement and is not
# part of this matrix.
#
# # How the stages are read
#
# Userspace output does not reach the serial port. The UKI command line ends
# `console=ttyS0,115200 console=tty0`, and the last `console=` wins for userspace — so
# the kernel's own messages arrive on serial and everything `plexos-init` prints goes to
# the virtual terminal. That is deliberate (a serial port the reference laptop does not
# have is a poor place for a diagnostic), and it means this script reads three separate
# channels:
#
#   serial   -- firmware and kernel, straight to a log file
#   screen   -- a QMP screendump of tty0, which is where PID 1 and plexosd write
#   network  -- whether the console answers, which is the strongest signal of all
#
# The network check is the one that matters most, because reaching it requires the
# Buildroot userspace to have worked: `plexosd` brings links up with `ip` and takes a
# lease with `udhcpc`, and both of those are Buildroot-compiled binaries. A machine
# whose console answers over the network has executed the exact class of binary a wrong
# CPU baseline kills.

set -uo pipefail

IMAGE="${1:?usage: cpu-boot-matrix.sh <image> <cpu-model> [seconds]}"
CPU="${2:?usage: cpu-boot-matrix.sh <image> <cpu-model> [seconds]}"
BUDGET="${3:-420}"

OVMF_CODE="${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
OVMF_VARS="${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cpu-matrix-${CPU}.XXXXXX")"
DISK="${WORK}/disk.img"
SERIAL="${WORK}/serial.log"
QMP="${WORK}/qmp.sock"
SHOT="${WORK}/screen.ppm"
# An unprivileged port, distinct per model so two runs cannot collide.
PORT=$(( 20000 + ($(cksum <<<"${CPU}" | cut -d' ' -f1) % 20000) ))

cleanup() { [ -n "${QPID:-}" ] && kill -9 "${QPID}" 2>/dev/null; }
trap cleanup EXIT

# A copy, never the image itself: the guest writes to /var and to the ESP, and the
# ESP write is evidence this script goes on to read.
cp --sparse=always "${IMAGE}" "${DISK}"
cp "${OVMF_VARS}" "${WORK}/vars.fd"

qemu-system-x86_64 \
    -machine q35,accel=tcg -cpu "${CPU}" -m 2560 -smp 2 \
    -drive if=pflash,format=raw,readonly=on,file="${OVMF_CODE}" \
    -drive if=pflash,format=raw,file="${WORK}/vars.fd" \
    -drive if=virtio,format=raw,file="${DISK}" \
    -netdev user,id=n0,hostfwd=tcp:127.0.0.1:${PORT}-:443 \
    -device virtio-net-pci,netdev=n0 \
    -display none -serial "file:${SERIAL}" \
    -qmp "unix:${QMP},server,nowait" \
    -no-reboot &
QPID=$!

# Poll the console rather than sleeping for the whole budget: a model that works
# finishes in a fraction of it, and the ones that do not are the ones worth waiting for.
SERVED=no
STATUS=""
for _ in $(seq 1 $(( BUDGET / 5 ))); do
    sleep 5
    kill -0 "${QPID}" 2>/dev/null || break
    STATUS="$(curl -sk -m 4 "https://127.0.0.1:${PORT}/api/status" 2>/dev/null)"
    if [ -n "${STATUS}" ]; then SERVED=yes; break; fi
done

# The virtual terminal, which is where userspace actually printed. Taken whether or not
# the console answered -- on a failed boot it is the only channel that has anything.
if kill -0 "${QPID}" 2>/dev/null && [ -S "${QMP}" ]; then
    # python3 rather than socat, which is not installed here and is one more thing a
    # person reproducing this would have to be told to install.
    timeout 20 python3 - "${QMP}" "${SHOT}" <<'PY' >/dev/null 2>&1
import json, socket, sys, time
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(8)
sock.connect(sys.argv[1])
sock.recv(65536)                                   # the greeting
sock.sendall(b'{"execute":"qmp_capabilities"}\n')
time.sleep(0.4); sock.recv(65536)
sock.sendall(json.dumps({"execute": "screendump",
                         "arguments": {"filename": sys.argv[2]}}).encode() + b"\n")
time.sleep(1.5); sock.recv(65536)
PY
fi

# QMP writes PPM. Convert, because a PNG is what anything else here can open, and the
# screen is the only record of what userspace printed on a boot that failed.
if [ -s "${SHOT}" ]; then
    python3 -c "from PIL import Image; Image.open('${SHOT}').save('${SHOT%.ppm}.png')" \
        2>/dev/null && SHOT="${SHOT%.ppm}.png"
fi

ALIVE=no; kill -0 "${QPID}" 2>/dev/null && ALIVE=yes
kill -9 "${QPID}" 2>/dev/null
wait "${QPID}" 2>/dev/null

# What the firmware and the kernel managed, off the serial log.
grep -qiE "BdsDxe|UEFI|EDK II" "${SERIAL}" 2>/dev/null && OVMF=yes || OVMF=no
grep -qE "Linux version" "${SERIAL}" 2>/dev/null && KERNEL=yes || KERNEL=no
PANIC=no; grep -qiE "Kernel panic|Attempted to kill init" "${SERIAL}" 2>/dev/null && PANIC=yes

printf 'cpu=%s mode=tcg ovmf=%s kernel=%s console_served=%s qemu_alive=%s panic=%s\n' \
       "${CPU}" "${OVMF}" "${KERNEL}" "${SERVED}" "${ALIVE}" "${PANIC}"
printf 'workdir=%s serial=%s screendump=%s\n' "${WORK}" "${SERIAL}" \
       "$( [ -s "${SHOT}" ] && echo "${SHOT}" || echo none )"
[ -n "${STATUS}" ] && printf 'status=%s\n' "${STATUS}"
trap - EXIT
exit 0
