#!/usr/bin/env bash
#
# Serve the update bundle post-image.sh produced, so an appliance can fetch it.
#
# This is the development half of network updates and it is deliberately trivial: a
# static HTTP server over one directory. Nothing signs anything, and whoever answers on
# this address chooses what /usr the appliance will run — which is why plexos-update
# treats the source as untrusted and relies on ADR-0005's rollback rather than on the
# transport. Do not run this anywhere but a bench.
#
#   tools/publish-update.sh [output-dir] [port]
#
# The appliance then does the rest from its console page, or by hand:
#
#   curl -X POST http://<appliance>/api/update \
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
    printf >&2 '%s has no update.json, so an appliance would refuse it\n' "${BUNDLE}"
    exit 1
}

# The version being served, said out loud. Publishing a bundle whose version does not
# sort above what the appliance runs is the one mistake that looks like success: the
# write completes, the entry is installed, and systemd-boot keeps choosing the old one.
VERSION=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "${BUNDLE}/update.json")
printf 'serving PlexOS %s from %s\n' "${VERSION}" "${BUNDLE}"
printf '  the appliance must be running something that sorts BELOW %s\n' "${VERSION}"
printf '  source URL: http://%s:%s/plexos-update\n\n' \
    "$(hostname -I 2>/dev/null | awk '{print $1}')" "${PORT}"

# --bind is not passed: the appliance is a different machine, so binding loopback only
# would serve nothing to the one client that matters.
cd "${OUTPUT}/images"
exec python3 -m http.server "${PORT}"
