#!/usr/bin/env bash
#
# Turn the bundle post-image.sh produced into one an appliance will install: write the
# ADR-0006 manifest and sign it.
#
#   tools/sign-bundle.sh <bundle-dir> <signing-key> <certificate-file>
#
# Deliberately not part of the build. The build runs on whatever host has the Buildroot
# tree; the signing key is the thing that must not be on every such host, and a build that
# needs it is a build that cannot be run without it. Keeping them apart also means a
# manifest is written once and never rewritten — ADR-0006's signature covers exact bytes,
# so a tool that reformats a signed manifest breaks it, and the safest way to never do that
# is to have exactly one thing that writes the file.
#
# What this reads is update.json, which post-image.sh writes and which carries the sizes,
# digests and root hash. What it writes is manifest.json and manifest.json.sig.
#
# Getting a key and a certificate, once:
#
#   cargo run -p plexos-update --bin plexos-sign -- root-key    ~/.plexos-keys/root-dev
#   cargo run -p plexos-update --bin plexos-sign -- signing-key ~/.plexos-keys/signing-dev
#   cargo run -p plexos-update --bin plexos-sign -- certify \
#       ~/.plexos-keys/root-dev ~/.plexos-keys/signing-dev plexos-signing-dev \
#       2028-01-01T00:00:00Z > ~/.plexos-keys/signing-dev.cert
#
# The root key's public half then has to be pasted into ROOT_KEYS and the image rebuilt,
# or no appliance will believe any of this. `plexos-sign trust` prints the constant.

set -euo pipefail

BUNDLE="${1:-}"
KEY="${2:-}"
CERT="${3:-}"

usage() {
    printf >&2 'usage: tools/sign-bundle.sh <bundle-dir> <signing-key> <certificate-file>\n'
    printf >&2 '  the bundle is <output>/images/medialith-update\n'
    exit 2
}

[ -n "${BUNDLE}" ] && [ -n "${KEY}" ] && [ -n "${CERT}" ] || usage

[ -f "${BUNDLE}/update.json" ] || {
    printf >&2 '%s has no update.json, so it is not a bundle post-image.sh produced\n' "${BUNDLE}"
    printf >&2 '  remedy: run a build; build_bundle is its last stage\n'
    exit 1
}
[ -f "${KEY}" ]  || { printf >&2 'no signing key at %s\n' "${KEY}"; exit 1; }
[ -f "${CERT}" ] || { printf >&2 'no certificate at %s\n' "${CERT}"; exit 1; }

REPO=$(cd "$(dirname "$0")/.." && pwd)
SIGN=(cargo run --quiet --manifest-path "${REPO}/Cargo.toml" -p plexos-update --bin plexos-sign --)

# ---------------------------------------------------------------------------
# The manifest
# ---------------------------------------------------------------------------
# Written by python rather than by a heredoc because every number in it comes out of
# another document, and a shell that gets one of them subtly wrong produces a manifest that
# signs and verifies and then fails a digest check after an 83 MB download.
python3 - "${BUNDLE}" "${CERT}" <<'PYTHON'
import base64, json, sys

bundle, cert_path = sys.argv[1], sys.argv[2]

with open(f"{bundle}/update.json") as f:
    described = json.load(f)

release = described["version"]
parts = release.split(".")
stamp = parts[-1] if len(parts) > 3 else ""
if len(parts) < 4 or len(stamp) != 12 or not stamp.isdigit():
    sys.exit(
        f"the bundle's version is {release}, which carries no YYYYMMDDHHMM build stamp.\n"
        "  The stamp is the manifest's anti-rollback sequence, so without one there is\n"
        "  nothing to stop an appliance being served this release again after a newer\n"
        "  one. Remedy: build with PLEXOS_VERSION=0.1.0.$(date -u +%Y%m%d%H%M)."
    )

# The certificate carries the key it authorises. Read it from there rather than taking it
# as an argument: a manifest whose key_id disagrees with its certificate is refused by the
# appliance, and there is no reason for a human to be able to introduce that disagreement.
body = base64.b64decode(open(cert_path).read().strip().split(".")[0])
key_id = json.loads(body)["key_id"]

def artifact(described, name):
    return {
        "size": described["size"],
        "sha256": described["sha256"],
        # A bare name, so this bundle can be served from any address without re-signing.
        # See plexos_update::location for why that is worth a rule about names.
        "sources": [{"kind": "full", "url": name}],
    }

manifest = {
    "manifest_version": 1,
    "product": "plexos",
    "channel": "dev",
    "os_version": ".".join(parts[:3]),
    "release": release,
    "sequence": int(stamp),
    "created_at": "{}-{}-{}T{}:{}:00Z".format(
        stamp[0:4], stamp[4:6], stamp[6:8], stamp[8:10], stamp[10:12]
    ),
    "usr": {
        "format": "erofs",
        "image": artifact(described["usr"], described["usr"]["name"]),
        "verity": {
            "root_hash": described["root_hash"],
            "hashes": artifact(described["verity"], described["verity"]["name"]),
        },
    },
    "uki": {
        "a": artifact(described["uki_a"], described["uki_a"]["name"]),
        "b": artifact(described["uki_b"], described["uki_b"]["name"]),
    },
    "signing": {
        "key_id": key_id,
        "certificate": open(cert_path).read().strip(),
    },
}

with open(f"{bundle}/manifest.json", "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")

print(f"manifest for {release}, sequence {stamp}, signed by {key_id}")
PYTHON

# ---------------------------------------------------------------------------
# The signature, and then the appliance's own opinion of it
# ---------------------------------------------------------------------------
"${SIGN[@]}" sign "${KEY}" "${BUNDLE}/manifest.json" > "${BUNDLE}/manifest.json.sig"

# The whole point of doing this here. Verifying with the same code the appliance runs,
# before publishing, is the difference between finding a mistake now and finding it as an
# update that will not install on a machine in another room.
ROOT="${PLEXOS_ROOT_KEY:-${KEY%/*}/root-dev}"
if [ -f "${ROOT}" ]; then
    "${SIGN[@]}" check "${ROOT}" "${BUNDLE}/manifest.json" "${BUNDLE}/manifest.json.sig"
else
    printf 'warning: no root key at %s, so the chain was not checked end to end\n' "${ROOT}"
    printf '  remedy: set PLEXOS_ROOT_KEY, or accept that the first machine to try this\n'
    printf '  bundle is the first thing to verify it\n'
fi

printf 'signed %s\n' "${BUNDLE}"
