#!/usr/bin/env bash
#
# Serve the update bundle post-image.sh produced, so an appliance can fetch it.
#
# This is the development half of network updates and it is deliberately trivial: a
# static HTTP server over one directory. The transport is not what makes it safe — the
# manifest is signed (ADR-0006) and the appliance refuses anything that does not chain to a
# root key in its own /usr, so whoever answers here can withhold an update but cannot
# choose one. Serving it is still a bench activity: there is no TLS and no access control.
#
#   tools/publish-update.sh [output-dir] [port]
#
# The appliance then does the rest from its console page, or by hand:
#
#   curl -k -X POST https://<appliance>/api/update \
#        -H "Authorization: Bearer <device token>" \
#        -H 'Content-Type: application/json' \
#        -d '{"install":true,"source":"http://<this host>:8080/plexos-update"}'

set -euo pipefail

OUTPUT="${1:-${PLEXOS_OUTPUT:-$(pwd)/output}}"
PORT="${2:-8080}"
BUNDLE="${OUTPUT}/images/plexos-update"

[ -d "${BUNDLE}" ] || {
    printf >&2 'no bundle at %s\n' "${BUNDLE}"
    printf >&2 '  remedy: run a build; post-image.sh writes it as its last stage\n'
    exit 1
}

[ -f "${BUNDLE}/update.json" ] || {
    printf >&2 '%s has no update.json, so it is not a bundle a build produced\n' "${BUNDLE}"
    exit 1
}

# The check that saves a round trip. An unsigned bundle is refused by the appliance with a
# perfectly clear message, but only after somebody has typed the address into a browser on
# another machine.
if [ ! -f "${BUNDLE}/manifest.json" ] || [ ! -f "${BUNDLE}/manifest.json.sig" ]; then
    printf >&2 '%s has no signed manifest, and an appliance will refuse it\n' "${BUNDLE}"
    printf >&2 '  remedy: tools/sign-bundle.sh %s <signing-key> <certificate>\n' "${BUNDLE}"
    exit 1
fi

# The version being served, said out loud. Publishing a bundle whose version does not
# sort above what the appliance runs is the one mistake that looks like success: the
# write completes, the entry is installed, and systemd-boot keeps choosing the old one.
VERSION=$(sed -n 's/.*"release": *"\([^"]*\)".*/\1/p' "${BUNDLE}/manifest.json")
SIGNER=$(sed -n 's/.*"key_id": *"\([^"]*\)".*/\1/p' "${BUNDLE}/manifest.json")
printf 'serving PlexOS %s from %s, signed by %s\n' "${VERSION}" "${BUNDLE}" "${SIGNER}"
printf '  the appliance must be running something that sorts BELOW %s\n' "${VERSION}"
printf '  source URL: http://%s:%s/plexos-update\n\n' \
    "$(hostname -I 2>/dev/null | awk '{print $1}')" "${PORT}"

# --bind is not passed: the appliance is a different machine, so binding loopback only
# would serve nothing to the one client that matters.
cd "${OUTPUT}/images"
exec python3 -m http.server "${PORT}"
