#!/usr/bin/env bash
#
# Make a release that was tested on one channel visible on another, without rebuilding it.
#
#   tools/promote-release.sh <tree> <release> <to-channel> <signing-key> <certificate>
#
# The whole point is what this does *not* do. It does not run a build, does not touch
# usr.erofs, usr.hash or either UKI, and does not copy an artefact. It writes one small
# manifest naming the same digests, signs it, and points a channel file at it — so the bytes
# that become stable are, provably, the bytes that were tested as dev.
#
# A manifest has to be re-signed rather than reused because `channel` is inside it and the
# signature covers the document's exact bytes. That is the correct trade: re-signing a four
# kilobyte document is cheap, and the alternative — a channel that lives outside the signed
# document — would let whoever serves the files decide which machines take a release.
#
# What it refuses:
#
#   * a release that is not in the tree, because promotion is not a way to publish;
#   * artefacts on disk that no longer match the digests the source manifest names, which is
#     the check that makes "the same bytes" a fact rather than an assumption.

set -euo pipefail

TREE="${1:-}"
RELEASE="${2:-}"
CHANNEL="${3:-}"
KEY="${4:-}"
CERT="${5:-}"

[ -n "${TREE}" ] && [ -n "${RELEASE}" ] && [ -n "${CHANNEL}" ] && [ -n "${KEY}" ] && [ -n "${CERT}" ] || {
    printf >&2 'usage: tools/promote-release.sh <tree> <release> <to-channel> <signing-key> <certificate>\n'
    exit 2
}

case "${CHANNEL}" in
    stable|beta|dev) ;;
    *) printf >&2 '%s is not a channel. Remedy: one of stable, beta, dev.\n' "${CHANNEL}"; exit 2 ;;
esac

[ -f "${KEY}" ]  || { printf >&2 'no signing key at %s\n' "${KEY}"; exit 1; }
[ -f "${CERT}" ] || { printf >&2 'no certificate at %s\n' "${CERT}"; exit 1; }

REPO=$(cd "$(dirname "$0")/.." && pwd)
SIGN=(cargo run --quiet --manifest-path "${REPO}/Cargo.toml" -p plexos-update --bin plexos-sign --)

DIRECTORY="${TREE}/releases/${RELEASE}"
[ -d "${DIRECTORY}" ] || {
    printf >&2 '%s is not in this tree, so there is nothing to promote\n' "${RELEASE}"
    printf >&2 '  remedy: publish it first with tools/publish-release.sh\n'
    exit 1
}

# The unsigned manifest first, into a temporary name: the signature covers exact bytes, so
# the document has to exist before it can be signed and must not be rewritten afterwards.
python3 - "${TREE}" "${RELEASE}" "${CHANNEL}" "${CERT}" <<'PYTHON'
import hashlib, glob, json, os, sys

tree, release, channel, cert_path = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
directory = os.path.join(tree, "releases", release)

sources = sorted(glob.glob(os.path.join(directory, "manifest-*.json")))
sources = [s for s in sources if not s.endswith(".sig")]
if not sources:
    sys.exit(f"{release} is in the tree with no manifest at all, so there is nothing to copy from")
source = json.load(open(sources[0]))

if source["channel"] == channel:
    print(f"{release} is already published to {channel}; re-signing it and repointing the channel")


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


# The check that makes this a promotion rather than a new release. Every artefact the
# manifest names is digested again, on disk, now -- so a tree somebody edited, a partial
# upload, or a release quietly rebuilt under the same name is refused here rather than
# discovered by an appliance after an 85 MB download.
for artifact in (
    source["usr"]["image"],
    source["usr"]["verity"]["hashes"],
    source["uki"]["a"],
    source["uki"]["b"],
):
    name = [s["url"] for s in artifact["sources"] if s.get("kind") == "full"][0]
    path = os.path.join(directory, name)
    if not os.path.exists(path):
        sys.exit(f"the manifest names {name} and the tree does not have it")
    found = digest(path)
    if found != artifact["sha256"]:
        sys.exit(
            f"{name} in the tree is not the file {release}'s manifest names.\n"
            f"  on disk:   {found}\n"
            f"  manifest:  {artifact['sha256']}\n"
            "  Refused. Promotion exists to publish the bytes that were tested, and these\n"
            "  are not those bytes."
        )
    print(f"  {name} matches the digest {release} was signed with")

import base64
body = base64.b64decode(open(cert_path).read().strip().split(".")[0])
key_id = json.loads(body)["key_id"]

promoted = dict(source)
promoted["channel"] = channel
promoted["signing"] = {"key_id": key_id, "certificate": open(cert_path).read().strip()}

with open(os.path.join(directory, f"manifest-{channel}.json"), "w") as f:
    json.dump(promoted, f, indent=2)
    f.write("\n")

print(f"manifest for {release} rewritten for {channel}, signed by {key_id}")
PYTHON

"${SIGN[@]}" sign "${KEY}" "${DIRECTORY}/manifest-${CHANNEL}.json" \
    > "${DIRECTORY}/manifest-${CHANNEL}.json.sig"

ROOT="${PLEXOS_ROOT_KEY:-${KEY%/*}/root-dev}"
if [ -f "${ROOT}" ]; then
    "${SIGN[@]}" check "${ROOT}" "${DIRECTORY}/manifest-${CHANNEL}.json" \
        "${DIRECTORY}/manifest-${CHANNEL}.json.sig"
else
    printf 'warning: no root key at %s, so the chain was not checked end to end\n' "${ROOT}"
fi

mkdir -p "${TREE}/channels"
printf '{\n  "release": "%s",\n  "manifest": "releases/%s/manifest-%s.json"\n}\n' \
    "${RELEASE}" "${RELEASE}" "${CHANNEL}" > "${TREE}/channels/${CHANNEL}.json"

printf 'promoted %s to %s\n' "${RELEASE}" "${CHANNEL}"
printf '  nothing was rebuilt: the /usr image, its hash tree and both boot images are the\n'
printf '  files that were already there, and their digests were checked against the manifest\n'
printf '  this release was signed with.\n'
