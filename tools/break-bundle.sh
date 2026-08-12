#!/usr/bin/env bash
#
# Produce a deliberately broken copy of an update bundle, to exercise ADR-0005's
# rollback on real hardware.
#
#   tools/break-bundle.sh <good-bundle-dir> <broken-bundle-dir> <version> \
#                         <signing-key> <certificate> --channel <channel>
#
# The rollback path is the one branch of this project that a bug turns from "degrades"
# into "bricks", and until it has actually run it is a claim rather than a fact. It
# cannot be tested by unit tests, because the thing under test is systemd-boot's
# counter, the firmware, and a kernel that has to give up three times.
#
# # What is broken, and why that particular thing
#
# The first 4 KiB block of usr.erofs is overwritten, and nothing else is touched. That
# block holds the erofs superblock, so it is read immediately on mount -- there is no
# way for the boot to get lucky and miss it.
#
# usr.hash is left exactly as it was, and so is the root hash on the UKI's command line.
# So dm-verity's table loads cleanly: the hash tree is intact and its root still matches
# what the signed-in-principle command line says it should be. What fails is the *first
# read*, where verity hashes the block it was given and finds it is not the block the
# tree describes. That is precisely the attack and precisely the accident that ADR-0004
# exists for -- a substituted or rotted /usr behind an intact hash tree -- so this tests
# verified boot and rollback in one go.
#
# The manifest's sha256 for usr.erofs is recomputed to match the broken file, and the
# manifest is then re-signed. That is not a workaround, it is the point: the updater must
# *accept* this bundle. It checks the signature, the sequence, the download digest and the
# partition after writing it, and every one of those should pass, because every one is
# asking "did I receive and store the bytes I was offered" and the answer is yes. Nothing
# in the update path is in a position to know the bytes are wrong. Only verity is, at boot,
# which is the whole design.
#
# Signing a bundle you have deliberately broken feels wrong and is exactly right: an
# experiment that skipped it would be testing the signature check, which is a different
# check that already has tests, and would prove nothing about ADR-0005.
#
# # Why the version is passed in rather than derived
#
# systemd-boot orders entries newest-first, so a broken bundle has to sort *above* what
# the appliance runs or it will be written, installed, and then never chosen -- an
# experiment that silently tests nothing. The version given here is what names the boot
# entry; the version inside the image is left alone, and the difference is invisible
# because this image is never going to get far enough to report anything.
#
# It must carry a build stamp, because the stamp is the anti-rollback sequence: a version
# below the floor the appliance holds is refused before anything is downloaded, which is
# the same silent nothing in a different disguise.
#
# # After the experiment
#
# The slot this was written to now holds an unbootable system, and the exhausted entry
# stays on the ESP where systemd-boot will keep skipping it. Publish a good bundle with
# a higher version to put that slot back, or the appliance is one bad update away from
# having no way home.

set -euo pipefail

GOOD="${1:-}"
BROKEN="${2:-}"
VERSION="${3:-}"
KEY="${4:-}"
CERT="${5:-}"
shift 5 2>/dev/null || true

# The channel is passed straight through to sign-bundle.sh, which has no default and will
# not invent one (ADR-0020). A broken bundle published to a channel the appliance under test
# does not track is refused before it is downloaded -- which is a perfectly correct refusal
# and a completely useless experiment, because rollback is never reached.
CHANNEL=""
while [ $# -gt 0 ]; do
    case "$1" in
        --channel) CHANNEL="${2:-}"; shift 2 ;;
        --channel=*) CHANNEL="${1#--channel=}"; shift ;;
        *) printf >&2 'unrecognised argument %s\n' "$1"; exit 2 ;;
    esac
done

if [ -z "${GOOD}" ] || [ -z "${BROKEN}" ] || [ -z "${VERSION}" ] \
   || [ -z "${KEY}" ] || [ -z "${CERT}" ] || [ -z "${CHANNEL}" ]; then
    printf >&2 'usage: %s <good-bundle-dir> <broken-bundle-dir> <version> <signing-key> <certificate> --channel <channel>\n' "$0"
    printf >&2 '  remedy: the first is output/images/medialith-update from a build; the\n'
    printf >&2 '          third must sort above what the appliance is running, and the\n'
    printf >&2 '          last two are what tools/sign-bundle.sh takes\n'
    exit 1
fi

case "${VERSION}" in
    *.*.*.????????????)
        ;;
    *)
        printf >&2 '%s carries no YYYYMMDDHHMM build stamp\n' "${VERSION}"
        printf >&2 '  remedy: the stamp is the anti-rollback sequence, and a bundle below\n'
        printf >&2 '          the appliance floor is refused before anything is downloaded\n'
        exit 1
        ;;
esac

[ -f "${GOOD}/update.json" ] || {
    printf >&2 'no update.json in %s, so that is not a bundle\n' "${GOOD}"
    printf >&2 '  remedy: point this at output/images/medialith-update\n'
    exit 1
}

[ -f "${GOOD}/usr.erofs" ] || {
    printf >&2 'no usr.erofs in %s\n' "${GOOD}"
    exit 1
}

rm -rf "${BROKEN}"
mkdir -p "${BROKEN}"
cp -a "${GOOD}/." "${BROKEN}/"

# 4096 bytes of a recognisable pattern over block 0. Recognisable so that if this ever
# turns up somewhere unexpected -- a slot nobody meant to break, a bundle that escaped a
# bench -- it says what it is rather than looking like corruption of unknown origin.
python3 - "${BROKEN}/usr.erofs" <<'PYTHON'
import sys

marker = b"PLEXOS-DELIBERATELY-BROKEN-ROLLBACK-TEST-BLOCK "
block = (marker * (4096 // len(marker) + 1))[:4096]

with open(sys.argv[1], "r+b") as image:
    image.seek(0)
    image.write(block)
PYTHON

DIGEST=$(sha256sum "${BROKEN}/usr.erofs" | awk '{print $1}')
SIZE=$(stat -c %s "${BROKEN}/usr.erofs")

# Rewritten with a JSON parser rather than sed. The manifest is small and the temptation
# to patch it with a regex is strong, but a mangled manifest fails at parse time on the
# appliance, which would look exactly like the broken image being caught early -- and
# the experiment would report success at proving nothing.
python3 - "${BROKEN}/update.json" "${VERSION}" "${DIGEST}" "${SIZE}" <<'PYTHON'
import json, sys

path, version, digest, size = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])

with open(path) as f:
    manifest = json.load(f)

manifest["version"] = version
manifest["usr"]["sha256"] = digest
manifest["usr"]["size"] = size

with open(path, "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
PYTHON

# Re-signed, so the appliance accepts it and only verity can object. The manifest is
# rewritten from the amended update.json rather than patched, for the reason above.
rm -f "${BROKEN}/manifest.json" "${BROKEN}/manifest.json.sig"
"$(dirname "$0")/sign-bundle.sh" "${BROKEN}" "${KEY}" "${CERT}" --channel "${CHANNEL}"

printf 'broken bundle at %s\n' "${BROKEN}"
printf '  version:   %s  (must sort above what the appliance runs)\n' "${VERSION}"
printf '  usr.erofs: block 0 overwritten, sha256 %s\n' "${DIGEST}"
printf '  usr.hash:  untouched, so verity loads and fails on the first read\n\n'
printf 'expected: three failed boots, then the previous slot\n'
