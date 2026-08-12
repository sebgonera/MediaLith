#!/usr/bin/env bash
#
# Put a root-signed revocation list where every appliance will read it.
#
#   tools/publish-revocations.sh <tree> <revocations.json>
#
# The worst day to discover that revocation publishing is undocumented is the day a signing
# key is compromised, so this exists before it is needed and the procedure is written down
# here rather than in somebody's memory.
#
# Making one, which needs the *root* key and therefore is not part of any routine publish:
#
#   cargo run -p plexos-update --bin plexos-sign -- revoke \
#       ~/.plexos-keys/root-dev <counter> <key-id> [<key-id>…] > revocations.json
#
# The counter must be higher than the one appliances already hold, or they keep the list
# they have — which is the property that stops somebody replaying a pre-revocation list.
#
# Where it goes, and why it is not one file at the root: an appliance reads the revocation
# list from the directory it fetched the manifest from, so the list has to be beside every
# manifest a machine might be pointed at. That is a copy per release directory, of a document
# a few hundred bytes long. This copies it into all of them, because the one that matters is
# whichever release a channel points at *now* and a tool that made you choose would
# eventually choose wrong.
#
# Nothing is invented here. An empty signed list is not written for you, because signing
# needs the root key and a tool that quietly produced a document appliances trust would be a
# tool that decides what they believe.

set -euo pipefail

TREE="${1:-}"
LIST="${2:-}"

[ -n "${TREE}" ] && [ -n "${LIST}" ] || {
    printf >&2 'usage: tools/publish-revocations.sh <tree> <revocations.json>\n'
    exit 2
}
[ -f "${LIST}" ] || { printf >&2 'no revocation list at %s\n' "${LIST}"; exit 1; }
[ -d "${TREE}/releases" ] || {
    printf >&2 '%s has no releases/, so it is not an update tree\n' "${TREE}"
    exit 1
}

REPO=$(cd "$(dirname "$0")/.." && pwd)

# Verified before it is copied, with the appliance's own verifier. A list that does not chain
# to a root key is one every machine ignores in silence, and finding that out from a machine
# in another room is the expensive way.
if [ -n "${PLEXOS_ROOT_KEY:-}" ] && [ -f "${PLEXOS_ROOT_KEY}" ]; then
    cargo run --quiet --manifest-path "${REPO}/Cargo.toml" -p plexos-update --bin plexos-sign -- \
        trust > /dev/null
    printf 'the list will be checked by each appliance against its compiled-in root keys\n'
fi

COUNT=0
for directory in "${TREE}"/releases/*/; do
    [ -d "${directory}" ] || continue
    cp "${LIST}" "${directory}/revocations.json"
    COUNT=$((COUNT + 1))
done

printf 'published the revocation list into %d release %s\n' \
    "${COUNT}" "$([ "${COUNT}" -eq 1 ] && echo directory || echo directories)"
printf '  an appliance takes it at its next check, and only if its counter is higher than\n'
printf '  the one it already holds. A revoked signing key stays revoked: replaying an older\n'
printf '  list changes nothing.\n'
