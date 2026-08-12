#!/usr/bin/env bash
#
# Runs representative Buildroot binaries out of one output tree under qemu-user on a
# range of CPU models, and reports which of them the model can actually execute.
#
#   tools/cpu-component-matrix.sh <buildroot-output-tree> [qemu-x86_64-static]
#
# These are **component** tests and not image tests. A binary starting under qemu-user
# says the instructions in the path it took are implemented by that CPU model; it says
# nothing about whether the image boots, which is what tools/cpu-boot-matrix.sh is for.
#
# Each binary is run through its own tree's dynamic loader so that the libraries it
# resolves are that tree's, not the host's. Without that, a target binary either fails
# to start or silently picks up the build machine's glibc, and the answer describes the
# wrong system.
#
# `--version` and `--help` are used deliberately: they are the shortest path through a
# program that still runs its startup, its loader relocations and its libc
# initialisation, which is where a baseline violation shows up. A program that gets that
# far on a given model has not proved every code path in it is safe -- only that the one
# it took is.

set -uo pipefail

TREE="${1:?usage: cpu-component-matrix.sh <buildroot-output-tree> [qemu]}"
QEMU="${2:-/home/sgonera/MediaLith/output/tmp/qemu/root/usr/bin/qemu-x86_64-static}"

MODELS=(Opteron_G1 Opteron_G2 Opteron_G3 Conroe Penryn Nehalem SandyBridge Haswell)

LOADER="${TREE}/target/lib/ld-linux-x86-64.so.2"
LIBPATH="${TREE}/target/lib:${TREE}/target/usr/lib"

if [ ! -x "${QEMU}" ]; then echo "no qemu-user at ${QEMU}" >&2; exit 2; fi
if [ ! -x "${LOADER}" ]; then echo "no loader at ${LOADER}" >&2; exit 2; fi

# name : path : arguments
CASES=(
  "busybox:${TREE}/target/bin/busybox:--help"
  "curl:${TREE}/target/usr/bin/curl:--version"
  "gpgv:${TREE}/target/usr/bin/gpgv:--version"
  "veritysetup:${TREE}/target/usr/sbin/veritysetup:--version"
  "mkfs.erofs:${TREE}/target/usr/sbin/mkfs.erofs:-V"
  "ip:${TREE}/target/sbin/ip:-V"
  "wpa_supplicant:${TREE}/target/usr/sbin/wpa_supplicant:-v"
)

printf '%-16s' "binary"
for m in "${MODELS[@]}"; do printf '%-13s' "${m}"; done
echo

for entry in "${CASES[@]}"; do
    name="${entry%%:*}"; rest="${entry#*:}"
    path="${rest%%:*}"; args="${rest#*:}"
    printf '%-16s' "${name}"
    if [ ! -x "${path}" ]; then
        for _ in "${MODELS[@]}"; do printf '%-13s' "absent"; done; echo; continue
    fi
    for model in "${MODELS[@]}"; do
        out="$( { timeout 60 "${QEMU}" -cpu "${model}" "${LOADER}" \
                    --library-path "${LIBPATH}" "${path}" "${args}"; } 2>&1 )"
        rc=$?
        # 132 is 128+SIGILL from the shell; qemu also prints the signal itself.
        if [ ${rc} -eq 132 ] || grep -qi "illegal instruction" <<<"${out}"; then
            printf '%-13s' "SIGILL"
        elif [ ${rc} -eq 124 ]; then
            printf '%-13s' "timeout"
        elif [ ${rc} -eq 0 ]; then
            printf '%-13s' "runs"
        else
            # Several of these exit non-zero on --version or --help by design. That is
            # not a CPU result, so it is reported as what it is rather than folded into
            # either column.
            printf '%-13s' "runs(rc=${rc})"
        fi
    done
    echo
done
