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
# verifies Plex's signature with gpgv, and provisioning builds an erofs app image.
#
# Plex transcoding is **not** measured and must not be. Plex ships its own executables
# and its own FFmpeg, is not rebuilt by this Buildroot flag, and dispatches on CPUID at
# run time — so its performance says nothing whatever about the toolchain's -march.
#
# # Why the binaries run natively
#
# Each tree's binaries are executed through that tree's own dynamic loader, on the host
# CPU. Both trees therefore run on the same processor with the same kernel and the same
# input, and the only difference is which instructions the compiler was permitted to
# emit. Running them under emulation instead would add a variable far larger than the
# effect being looked for.
#
# # Three things this got wrong on its first run, all of which flattered the result
#
# 1. It timed a command that was not running. mkfs.erofs is in target/usr/bin, not
#    target/usr/sbin, so the case exited 127 in twelve milliseconds and was reported as
#    a timing. Every case now has its exit status checked before anything is measured,
#    and a case that cannot run says so instead of producing a number.
# 2. Tree A was measured to completion before tree B started, so A paid every cold-cache
#    cost and B paid none. busybox came out "55% faster" on that alone. The runs are
#    interleaved now.
# 3. There was no warm-up, and the workloads were small enough that process startup and
#    page-cache state dominated. Both fixed.

set -uo pipefail

A="${1:?usage: cpu-baseline-bench.sh <tree-a> <tree-b> [repeats]}"
B="${2:?usage: cpu-baseline-bench.sh <tree-a> <tree-b> [repeats]}"
REPEATS="${3:-7}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cpu-bench.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT

# Fixed inputs, made once and shared, so both trees do identical work.
head -c $((512 * 1024 * 1024)) /dev/urandom > "${WORK}/blob"
mkdir -p "${WORK}/tree"
for i in $(seq 1 400); do head -c $((512 * 1024)) /dev/urandom > "${WORK}/tree/f${i}"; done

run_in() {
    local tree="$1"; shift
    local loader="${tree}/target/lib/ld-linux-x86-64.so.2"
    [ -x "${loader}" ] || return 127
    "${loader}" --library-path "${tree}/target/lib:${tree}/target/usr/lib" "$@"
}

sha256_case()       { run_in "$1" "$1/target/bin/busybox" sha256sum "${WORK}/blob"; }
gpgv_case()         { run_in "$1" "$1/target/usr/bin/gpgv" --version; }
erofs_case()        { rm -f "${WORK}/out.erofs"
                      run_in "$1" "$1/target/usr/bin/mkfs.erofs" -zlz4hc \
                             "${WORK}/out.erofs" "${WORK}/tree"; }
busybox_start_case(){ local i; for i in $(seq 1 400); do
                          run_in "$1" "$1/target/bin/busybox" true || return 1; done; }
veritysetup_case()  { run_in "$1" "$1/target/usr/sbin/veritysetup" --version; }

ms_of() {
    local start end
    start=$(date +%s%N)
    "$@" >/dev/null 2>&1
    local rc=$?
    end=$(date +%s%N)
    [ ${rc} -eq 0 ] || return ${rc}
    echo $(( (end - start) / 1000000 ))
}

median() { printf '%s\n' "$@" | sort -n | awk '{v[NR]=$1} END{print v[int((NR+1)/2)]}'; }

bench_case() {
    local label="$1" fn="$2"
    # Both trees must actually run the case, or there is nothing to compare. This is
    # checked before any timing, because the failure it catches produced a number that
    # looked like a very fast success.
    if ! "${fn}" "${A}" >/dev/null 2>&1; then
        printf '%-26s  %-11s %-11s %s\n' "${label}" "-" "-" "case does not run in A"
        return
    fi
    if ! "${fn}" "${B}" >/dev/null 2>&1; then
        printf '%-26s  %-11s %-11s %s\n' "${label}" "-" "-" "case does not run in B"
        return
    fi
    local -a ta=() tb=() ; local i v
    for i in $(seq 1 "${REPEATS}"); do
        # Interleaved, so cache and thermal state are shared rather than paid by
        # whichever tree happened to go first.
        v=$(ms_of "${fn}" "${A}") && ta+=("${v}")
        v=$(ms_of "${fn}" "${B}") && tb+=("${v}")
    done
    [ ${#ta[@]} -eq 0 ] || [ ${#tb[@]} -eq 0 ] && : # keep set -u happy
    local ma mb delta
    ma=$(median "${ta[@]}"); mb=$(median "${tb[@]}")
    delta=$(awk -v a="${ma}" -v b="${mb}" 'BEGIN{ if (a==0) print "n/a";
                 else printf "%+.1f%%", (b-a)*100.0/a }')
    printf '%-26s  %-11s %-11s %s\n' "${label}" "${ma} ms" "${mb} ms" "${delta}"
}

printf 'repeats=%d (interleaved, median)\n  A=%s\n  B=%s\n\n' "${REPEATS}" "${A}" "${B}"
# One untimed pass of the heaviest case, to get the input into the page cache before
# anything is measured.
sha256_case "${A}" >/dev/null 2>&1
printf '%-26s  %-11s %-11s %s\n' "case" "A" "B" "B vs A"
printf '%-26s  %-11s %-11s %s\n' "----" "-" "-" "------"
bench_case "sha256sum 512 MiB"   sha256_case
bench_case "mkfs.erofs 200 MiB"  erofs_case
bench_case "busybox spawn x400"  busybox_start_case
bench_case "gpgv startup"        gpgv_case
bench_case "veritysetup startup" veritysetup_case
