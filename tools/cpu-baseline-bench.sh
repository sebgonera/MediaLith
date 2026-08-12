#!/usr/bin/env bash
#
# Compares two Buildroot output trees on the work the appliance actually does.
#
#   tools/cpu-baseline-bench.sh <tree-a> <tree-b> [repeats]
#
# # What is measured, and what is deliberately not
#
# Only appliance-owned work: hashing, signature verification, and building an app
# image. Those are the three things MediaLith does with its own userspace where a CPU
# baseline could plausibly matter — update verification hashes a partition, ADR-0010
# verifies Plex's signature with gpgv, and provisioning builds an erofs image.
#
# Plex transcoding is **not** measured and must not be. Plex ships its own executables
# and its own FFmpeg, is not rebuilt by this Buildroot flag, and dispatches on CPUID at
# run time — so its performance says nothing whatever about the toolchain's -march.
#
# # Why the binaries run natively
#
# Each tree's binaries are executed through that tree's own dynamic loader, on the host
# CPU. Both trees therefore run on the same i7-14700 with the same kernel and the same
# input, and the only difference is which instructions the compiler was permitted to
# emit. Running them under emulation instead would add a variable far larger than the
# effect being looked for.

set -uo pipefail

A="${1:?usage: cpu-baseline-bench.sh <tree-a> <tree-b> [repeats]}"
B="${2:?usage: cpu-baseline-bench.sh <tree-a> <tree-b> [repeats]}"
REPEATS="${3:-5}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cpu-bench.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT

# A fixed input, made once and shared, so the two trees hash the same bytes.
head -c $((256 * 1024 * 1024)) /dev/urandom > "${WORK}/blob"
mkdir -p "${WORK}/tree"
for i in $(seq 1 200); do head -c $((512 * 1024)) /dev/urandom > "${WORK}/tree/f${i}"; done

# Run a target binary out of tree $1 through that tree's own loader.
run_in() {
    local tree="$1"; shift
    local loader="${tree}/target/lib/ld-linux-x86-64.so.2"
    [ -x "${loader}" ] || return 127
    "${loader}" --library-path "${tree}/target/lib:${tree}/target/usr/lib" "$@"
}

# Median wall-clock of REPEATS runs, in milliseconds.
median_ms() {
    local -a times=()
    local i start end
    for i in $(seq 1 "${REPEATS}"); do
        start=$(date +%s%N)
        "$@" >/dev/null 2>&1
        end=$(date +%s%N)
        times+=( $(( (end - start) / 1000000 )) )
    done
    printf '%s\n' "${times[@]}" | sort -n | awk '{v[NR]=$1} END{print v[int((NR+1)/2)]}'
}

bench_case() {
    local label="$1"; shift
    local a b delta
    a=$(median_ms "$@" "${A}") ; b=$(median_ms "$@" "${B}")
    if [ -z "${a}" ] || [ -z "${b}" ] || [ "${a}" = "0" ]; then
        printf '%-28s  %-12s %-12s %s\n' "${label}" "${a:-n/a}" "${b:-n/a}" "not comparable"
        return
    fi
    delta=$(awk -v a="${a}" -v b="${b}" 'BEGIN{printf "%+.1f%%", (b-a)*100.0/a}')
    printf '%-28s  %-12s %-12s %s\n' "${label}" "${a} ms" "${b} ms" "${delta}"
}

sha256_case() { run_in "$1" "$1/target/bin/busybox" sha256sum "${WORK}/blob"; }
gpgv_case()   { run_in "$1" "$1/target/usr/bin/gpgv" --version; }
erofs_case()  {
    rm -f "${WORK}/out.erofs"
    run_in "$1" "$1/target/usr/sbin/mkfs.erofs" -zlz4hc "${WORK}/out.erofs" "${WORK}/tree"
}
busybox_start_case() {
    local i
    for i in $(seq 1 200); do run_in "$1" "$1/target/bin/busybox" true; done
}

printf 'repeats=%d  A=%s  B=%s\n\n' "${REPEATS}" "${A}" "${B}"
printf '%-28s  %-12s %-12s %s\n' "case" "A" "B" "B vs A"
printf '%-28s  %-12s %-12s %s\n' "----" "-" "-" "------"
bench_case "sha256sum 256 MiB"      sha256_case
bench_case "mkfs.erofs 100 MiB"     erofs_case
bench_case "busybox spawn x200"     busybox_start_case
bench_case "gpgv startup"           gpgv_case
