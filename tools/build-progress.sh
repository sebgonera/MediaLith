#!/usr/bin/env bash
#
# How far along is the Buildroot build?
#
# Buildroot itself will not tell you. It prints a wall of compiler invocations and
# nothing that answers "how much is left", which on a four-hour first build on two
# cores is the only question anyone actually has.
#
#   tools/build-progress.sh              one-shot
#   tools/build-progress.sh --watch      refreshes until the build stops
#
# The output directory is taken from $PLEXOS_OUTPUT, then $1, then ./output.
#
# ---------------------------------------------------------------------------
# What the percentage means, and what it does not
# ---------------------------------------------------------------------------
#
# Packages are not equal. host-gcc-final alone outweighs thirty of the small ones,
# so counting finished packages produces a bar that races to 80% and then sits
# still for two hours -- worse than no bar, because it implies an ETA it cannot
# support. The weights below are rough relative build costs, and the bar is
# weighted by them.
#
# They are estimates. The bar is honest about direction and rough magnitude; it is
# not a promise about minutes.

set -uo pipefail

OUTPUT="${PLEXOS_OUTPUT:-${1:-$(pwd)/output}}"
[ "${OUTPUT}" = "--watch" ] && OUTPUT="$(pwd)/output"
BUILD="${OUTPUT}/build"
LOG="$(dirname "${OUTPUT}")/build.log"

WATCH=0
for arg in "$@"; do [ "${arg}" = "--watch" ] && WATCH=1; done

if [ ! -d "${BUILD}" ]; then
    printf 'No Buildroot output at %s\n' "${OUTPUT}" >&2
    printf 'Set PLEXOS_OUTPUT or pass the output directory as an argument.\n' >&2
    exit 1
fi

# Relative build cost. Anything unlisted gets DEFAULT_WEIGHT, which suits the many
# small library packages. Only the outliers need naming.
weight_for() {
    case "$1" in
        host-gcc-final|gcc-final)          echo 120 ;;
        linux)                             echo 120 ;;
        host-gcc-initial)                  echo 100 ;;
        glibc)                             echo  90 ;;
        mesa3d)                            echo  80 ;;
        intel-mediadriver)                 echo  40 ;;
        host-binutils|binutils)            echo  30 ;;
        plexos-systemd-boot)               echo  25 ;;
        linux-firmware)                    echo  20 ;;
        host-python3|python3)              echo  20 ;;
        xfsprogs|cryptsetup|host-cryptsetup) echo 12 ;;
        busybox|host-fakeroot|libva|intel-gmmlib) echo 10 ;;
        *)                                 echo   5 ;;
    esac
}

# `show-targets` lists only what the configuration asks for. Buildroot builds a good
# deal more: the internal toolchain steps, and the host tools packages drag in
# implicitly -- host-cmake, host-meson, host-python3 and its whole dependency chain.
# There were 37 such packages in one measured build against 8 named here, which is
# why this list is a floor and not the answer.
#
# The real total is the union of this list, `show-targets`, and every package that
# has appeared in build/. Taking the union guarantees the finished set is a subset of
# the total, so the count can never read "84 of ~73" and the bar can never exceed
# 100% -- which is what happened when the total was a fixed guess.
#
# It understates early on, when packages that will be built have not appeared yet, so
# the bar drifts down slightly as they show up. That is the honest direction to be
# wrong in: it never claims to be further along than it is.
TOOLCHAIN_EXTRAS="host-binutils host-gcc-initial host-gcc-final host-gmp host-mpc host-mpfr host-bison host-gawk"

# Every package Buildroot has actually started, finished or not.
started_packages() {
    local d
    for d in "${BUILD}"/*/; do
        [ -d "${d}" ] || continue
        basename "${d}" | sed -E 's/-[0-9].*$//'
    done
}

# `show-targets` needs a configured tree and takes a moment, so cache it. It only
# changes when the defconfig does.
targets_list() {
    local cache="${OUTPUT}/.build-progress-targets"
    if [ -s "${cache}" ] && [ "${cache}" -nt "${OUTPUT}/.config" ]; then
        cat "${cache}"
        return
    fi

    # Generating it needs `make show-targets`, and that must NOT run against
    # ${OUTPUT}: a second make on a directory that already has a build running in
    # it can corrupt both. So configure a throwaway tree and ask that one instead.
    local br="${BR2_BUILDROOT:-$(dirname "${OUTPUT}")/buildroot-upstream}"
    local ext="${BR2_EXTERNAL:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../buildroot" && pwd)}"
    local scratch="${OUTPUT}.targets-scratch"

    if [ -d "${br}" ] && [ -d "${ext}" ]; then
        rm -rf "${scratch}"
        mkdir -p "${scratch}"
        if make -C "${br}" BR2_EXTERNAL="${ext}" O="${scratch}" \
                plexos_x86_64_defconfig >/dev/null 2>&1; then
            make -C "${br}" BR2_EXTERNAL="${ext}" O="${scratch}" show-targets 2>/dev/null \
                | tr ' ' '\n' | grep -v '^$' | sort -u > "${cache}"
        fi
        rm -rf "${scratch}"
    fi

    # An empty cache is not fatal: the bar falls back to the packages already on
    # disk, which understates the total and so overstates progress. Say so rather
    # than quietly showing a wrong number.
    if [ -s "${cache}" ]; then
        cat "${cache}"
    else
        printf 'TARGETS-UNKNOWN\n'
    fi
}

is_running() { pgrep -f "O=${OUTPUT}" >/dev/null 2>&1 || pgrep -f 'buildroot-upstream' >/dev/null 2>&1; }

# A package counts as done when it has any *installed stamp.
done_packages() {
    local d
    for d in "${BUILD}"/*/; do
        [ -d "${d}" ] || continue
        if compgen -G "${d}.stamp_*installed" >/dev/null 2>&1; then
            basename "${d}" | sed -E 's/-[0-9].*$//'
        fi
    done
}

current_package() {
    ls -dt "${BUILD}"/*/ 2>/dev/null | head -1 | xargs -r basename | sed -E 's/-[0-9].*$//'
}

human_time() {
    local s=$1
    printf '%dh %02dm' $((s / 3600)) $(((s % 3600) / 60))
}

bar() {
    local pct=$1 width=42 filled
    filled=$((pct * width / 100))
    [ "${filled}" -gt "${width}" ] && filled=${width}
    printf '['
    printf '%0.s#' $(seq 1 "${filled}") 2>/dev/null
    printf '%0.s-' $(seq 1 $((width - filled))) 2>/dev/null
    printf ']'
}

report() {
    local all_targets done_list total_weight done_weight pct n_done n_total
    all_targets="$(targets_list)"
    # Union of: what the config asks for, the toolchain steps it omits, and
    # everything already on disk. The last term is what makes the total impossible
    # to undershoot.
    all_targets="$(printf '%s\n%s\n%s\n' \
        "${all_targets}" \
        "$(printf '%s\n' ${TOOLCHAIN_EXTRAS})" \
        "$(started_packages)" | grep -v '^$' | sort -u)"
    done_list="$(done_packages | sort -u)"

    local unknown=0
    if printf '%s\n' "${all_targets}" | grep -q '^TARGETS-UNKNOWN$'; then
        unknown=1
        all_targets="$(printf '%s\n' "${all_targets}" | grep -v '^TARGETS-UNKNOWN$')"
    fi

    total_weight=0
    n_total=0
    while read -r pkg; do
        [ -z "${pkg}" ] && continue
        total_weight=$((total_weight + $(weight_for "${pkg}")))
        n_total=$((n_total + 1))
    done <<< "${all_targets}"

    done_weight=0
    n_done=0
    while read -r pkg; do
        [ -z "${pkg}" ] && continue
        done_weight=$((done_weight + $(weight_for "${pkg}")))
        n_done=$((n_done + 1))
    done <<< "${done_list}"

    if [ "${total_weight}" -le 0 ]; then
        pct=0
    else
        pct=$((done_weight * 100 / total_weight))
        [ "${pct}" -gt 100 ] && pct=100
    fi

    local started elapsed
    started=$(stat -c %W "${LOG}" 2>/dev/null || echo 0)
    [ "${started}" = "0" ] && started=$(stat -c %Y "${OUTPUT}/.config" 2>/dev/null || echo 0)
    if [ "${started}" != "0" ]; then
        elapsed=$(( $(date +%s) - started ))
    else
        elapsed=0
    fi

    local status errors
    if is_running; then status="building"; else status="STOPPED"; fi
    # grep -c prints its count AND exits non-zero when that count is zero, so a
    # `|| echo 0` here appends a second zero and makes the check below fire on a
    # clean build.
    errors=$(grep -cE 'Error [0-9]+$' "${LOG}" 2>/dev/null)
    errors=${errors:-0}

    printf '\n  PlexOS build  %s  %s%%\n' "$(bar "${pct}")" "${pct}"
    if [ "${unknown}" -eq 1 ]; then
        printf '  %-14s %s\n' "packages" "${n_done} done; TOTAL UNKNOWN, so this bar reads high"
    else
        printf '  %-14s %s\n' "packages" "${n_done} of ~${n_total} complete"
    fi
    printf '  %-14s %s\n' "now building" "$(current_package)"
    printf '  %-14s %s\n' "elapsed" "$(human_time "${elapsed}")"
    printf '  %-14s %s\n' "status" "${status}"
    # The log accumulates across resumes, so a count here can be entirely historical.
    # A build that is currently running has, by definition, got past whatever those
    # errors were -- saying "errors 1" without that distinction reads as a live
    # failure and sends you off reading a log about something already fixed.
    if [ "${status}" = "building" ] && [ "${errors}" != "0" ]; then
        printf '  %-14s %s\n' "errors" "${errors} earlier in this log, none blocking now"
    else
        printf '  %-14s %s\n' "errors" "${errors}"
    fi
    printf '  %-14s %s\n' "disk used" "$(du -sh "${OUTPUT}" 2>/dev/null | cut -f1)"

    if [ "${errors}" != "0" ]; then
        printf '\n  Last error:\n'
        grep -E 'Error [0-9]+$' "${LOG}" 2>/dev/null | tail -1 | sed 's/^/    /'
    fi
    if [ "${status}" = "STOPPED" ] && [ "${pct}" -lt 100 ]; then
        printf '\n  The build is not running. Resume it with:\n'
        printf '    cd %s && make O=%s\n' "$(dirname "${OUTPUT}")/buildroot-upstream" "${OUTPUT}"
    fi
    printf '\n'
}

if [ "${WATCH}" -eq 1 ]; then
    while true; do
        clear
        report
        printf '  refreshing every 30s, Ctrl-C to stop\n'
        is_running || break
        sleep 30
    done
else
    report
fi
