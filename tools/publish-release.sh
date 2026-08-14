#!/usr/bin/env bash
#
# Put a signed bundle into a static update tree, so an appliance can find it by itself.
#
#   tools/publish-release.sh <tree> <bundle-dir> [revocations.json]
#
# The tree is the thing an update service serves, and it is only files (ADR-0020):
#
#   <tree>/channels/dev.json                     {"release": "…", "manifest": "releases/…"}
#   <tree>/releases/<release>/manifest-dev.json  and .sig
#   <tree>/releases/<release>/usr.erofs  usr.hash  plexos-<release>-a.efi  -b.efi
#   <tree>/releases/<release>/revocations.json   if there is one
#
# No database, no application, no authentication. rsync it to a web server or upload it to
# object storage; the signature over the manifest is what makes it safe, and the manifest
# addresses its artefacts by bare names so the same bytes work at any address.
#
# Two rules this enforces, both of which are about the same thing:
#
#   A release identifier names bytes. Publishing 0.1.1.2026… twice with different artefacts
#   would make that string mean two operating systems, so a second publish of a release
#   already in the tree must present identical digests or it is refused.
#
#   The artefacts are stored once per release, not once per channel. Promotion re-signs a
#   small manifest beside them (tools/promote-release.sh) and copies nothing, which is what
#   makes "the bytes that were tested are the bytes that become stable" a fact rather than a
#   hope.

set -euo pipefail

TREE="${1:-}"
BUNDLE="${2:-}"
REVOCATIONS="${3:-}"

[ -n "${TREE}" ] && [ -n "${BUNDLE}" ] || {
    printf >&2 'usage: tools/publish-release.sh <tree> <bundle-dir> [revocations.json]\n'
    printf >&2 '  the bundle is <output>/images/medialith-update, after tools/sign-bundle.sh\n'
    exit 2
}

for name in manifest.json manifest.json.sig; do
    [ -f "${BUNDLE}/${name}" ] || {
        printf >&2 '%s has no %s, so it has not been signed\n' "${BUNDLE}" "${name}"
        printf >&2 '  remedy: tools/sign-bundle.sh %s <key> <cert> --channel <channel>\n' "${BUNDLE}"
        exit 1
    }
done

python3 - "${TREE}" "${BUNDLE}" "${REVOCATIONS}" <<'PYTHON'
import hashlib, json, os, shutil, sys

tree, bundle, revocations = sys.argv[1], sys.argv[2], sys.argv[3]

manifest = json.load(open(f"{bundle}/manifest.json"))
release, channel = manifest["release"], manifest["channel"]
if channel not in ("stable", "beta", "dev"):
    sys.exit(
        f"the manifest names channel {channel!r}, which no appliance tracks.\n"
        "  Remedy: sign it again with --channel stable|beta|dev."
    )


def artifacts(manifest):
    """Every file the manifest names, by the bare name it names it with."""
    found = []
    for artifact in (
        manifest["usr"]["image"],
        manifest["usr"]["verity"]["hashes"],
        manifest["uki"]["a"],
        manifest["uki"]["b"],
    ):
        names = [s["url"] for s in artifact["sources"] if s.get("kind") == "full"]
        if not names:
            sys.exit("an artefact in this manifest has no full source, so it cannot be published")
        found.append((names[0], artifact["sha256"], artifact["size"]))
    return found


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


directory = os.path.join(tree, "releases", release)
os.makedirs(directory, exist_ok=True)
os.makedirs(os.path.join(tree, "channels"), exist_ok=True)

for name, sha256, size in artifacts(manifest):
    source = os.path.join(bundle, name)
    if not os.path.exists(source):
        sys.exit(f"the manifest names {name} and the bundle does not have it")

    # The bundle is checked against its own manifest before anything is compared to the
    # tree, and this order was arrived at by getting it wrong. A bundle whose artefact was
    # rebuilt after signing carries a manifest describing the *previous* file; the tree
    # already holds that previous file, so the two digests agree, the copy is skipped as
    # "already published and identical", and a channel ends up pointing at a release made of
    # bytes nobody published. Every check passed and the wrong thing happened.
    offered = digest(source)
    if offered != sha256:
        sys.exit(
            f"the bundle's {name} is not the file its own manifest names.\n"
            f"  on disk:   {offered}\n"
            f"  manifest:  {sha256}\n"
            "  Refused. Something rebuilt an artefact after the bundle was signed.\n"
            "  Remedy: sign it again -- tools/sign-bundle.sh reads the build's update.json,\n"
            "  so re-run the build stage that writes it if the artefact really did change."
        )

    target = os.path.join(directory, name)

    if os.path.exists(target):
        # The immutable-release rule, enforced against the bytes rather than against a
        # record of them. A release identifier that named two different operating systems
        # would make every other guarantee in this system unverifiable: an appliance
        # reporting "running 0.1.1" would not be saying which 0.1.1.
        already = digest(target)
        if already != sha256:
            sys.exit(
                f"{release} is already published and its {name} is a different file.\n"
                f"  published: {already}\n"
                f"  offered:   {sha256}\n"
                "  Refused. A release identifier names bytes, and republishing one with\n"
                "  different bytes makes it name two operating systems -- after which no\n"
                "  appliance can say which of them it is running.\n"
                "  Remedy: build a new release. PLEXOS_VERSION=0.1.0.$(date -u +%Y%m%d%H%M)."
            )
        print(f"  {name} is already published and identical")
        continue

    shutil.copyfile(source, target)
    # Checked after the copy rather than trusted from the manifest: a short write here
    # publishes a release that every appliance downloads and then refuses, and the message
    # they would print blames the download.
    written = digest(target)
    if written != sha256 or os.path.getsize(target) != size:
        sys.exit(f"{name} did not copy correctly into the tree: got {written}, expected {sha256}")
    print(f"  {name} published ({size // 1_000_000} MB)")

# The manifest is named for its channel, and lives beside the artefacts because their
# sources are bare names resolved against wherever the manifest was fetched from.
manifest_name = f"manifest-{channel}.json"
shutil.copyfile(f"{bundle}/manifest.json", os.path.join(directory, manifest_name))
shutil.copyfile(f"{bundle}/manifest.json.sig", os.path.join(directory, manifest_name + ".sig"))

if revocations:
    if not os.path.exists(revocations):
        sys.exit(f"no revocation list at {revocations}")
    shutil.copyfile(revocations, os.path.join(directory, "revocations.json"))
    print("  revocations.json published beside the manifest")

with open(os.path.join(tree, "channels", f"{channel}.json"), "w") as f:
    json.dump({"release": release, "manifest": f"releases/{release}/{manifest_name}"}, f, indent=2)
    f.write("\n")

print(f"published {release} to the {channel} channel")
print(f"  appliances tracking {channel} will offer it at the next check")
PYTHON

printf '\nserve the tree with:\n'
printf '  python3 -m http.server 8080 --directory %s\n' "${TREE}"
printf 'and set that address as the update service in the console (System → System updates).\n'
