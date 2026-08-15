#!/usr/bin/env bash
#
# Assemble a Technical Preview release from a finished build. It does not publish.
#
#   tools/make-preview-release.sh <buildroot-output-dir> <preview-number> [out-dir]
#
# Example:
#   tools/make-preview-release.sh output-generic 1
#
# What it produces, in `<out-dir>` (default `release/`):
#
#   MediaLith-<os-version>-technical-preview-<n>-x86_64.img.xz
#   SHA256SUMS
#   RELEASE-NOTES.md
#
# # Why the public name and the internal one differ
#
# The asset is called `MediaLith-…`; the image the build writes is `medialith.img` and the
# boot entries inside it are `plexos-<release>.efi`. That is not an oversight and it is not
# half a rename: ADR-0022 froze `plexos` as an internal namespace because those names are
# contracts with disks and with releases already installed. The public artefact is branding
# and can say anything; the boot entry cannot.
#
# The **build stamp goes in the notes and in SHA256SUMS**, never only in the file name. A
# person reporting a fault says "Technical Preview 1"; the only thing that identifies what
# they actually ran is `0.1.0.YYYYMMDDHHMM`, and it has to be somewhere they can find it.
#
# # What it proves about the source, and why a clean checkout is not enough
#
# A release names the commit it came from, and that claim has to be true. Checking `git
# status` proves only that the *checkout* is tidy — it says nothing about which source
# produced the image sitting in `output-generic/`, and those two come apart the moment an
# output tree outlives the commit that filled it. A stale image packaged against a newer
# commit would pass every check and name a commit that never produced it.
#
# So the commit is recorded *inside the image* at build time (`post-image.sh` writes
# `MEDIALITH_COMMIT` into `os-release` before the /usr erofs is built, which puts it under
# dm-verity), and this reads it back out of the built image. Four things must agree before
# anything is packaged: the image records a commit, it was built from a clean tree, that
# commit is HEAD, and the tree is still clean.
#
# `--allow-dirty` bypasses all four for local experiments and stamps the notes
# unreproducible, so the output cannot be mistaken for a release.
#
# # Why the image compresses so well
#
# It is 8 GiB and mostly zeros -- `/var` is a freshly formatted XFS that fills on first
# boot. Measured: 8.00 GiB apparent, 377 MB actually allocated, 194 MB after `xz -9`.

set -uo pipefail

OUTPUT="${1:?usage: make-preview-release.sh <buildroot-output-dir> <preview-number> [out-dir]}"
PREVIEW="${2:?usage: make-preview-release.sh <buildroot-output-dir> <preview-number> [out-dir]}"
DEST="${3:-release}"
ALLOW_DIRTY="${ALLOW_DIRTY:-0}"
[ "${4:-}" = "--allow-dirty" ] && ALLOW_DIRTY=1

die() { printf '\nmake-preview-release: %s\n' "$1" >&2; exit 1; }
say() { printf '>>> %s\n' "$1"; }

case "${PREVIEW}" in
    ''|*[!0-9]*) die "the preview number must be a number, got ${PREVIEW}" ;;
esac

IMAGE="${OUTPUT}/images/medialith.img"
UPDATE="${OUTPUT}/images/medialith-update/update.json"
[ -r "${IMAGE}" ]  || die "no image at ${IMAGE}. Build first: make -C ../buildroot-upstream O=\$(pwd)/${OUTPUT} all"
[ -r "${UPDATE}" ] || die "no ${UPDATE}; the build did not finish"

command -v xz >/dev/null       || die "xz is not installed"
command -v sha256sum >/dev/null || die "sha256sum is not installed"

# ------------------------------------------------------- what the image says about itself
#
# Read out of the built image, not out of the working tree. That distinction is the whole
# point of this block.
#
# A clean `git status` proves the *checkout* is tidy. It says nothing about which source
# produced `medialith.img`, and the two come apart easily: an output tree outlives the
# commit that filled it, so a release assembled from a stale `output-generic/` while the
# repository sits on a newer commit would name a commit that never produced those bytes.
# Every check downstream would pass and the provenance would be fiction.
#
# So `post-image.sh` writes `MEDIALITH_COMMIT` into `os-release` before the /usr image is
# built, which puts it inside the erofs that dm-verity covers, and this reads it back from
# there. The image is asked what it is; the repository is not asked to vouch for it.
USR_EROFS="${OUTPUT}/images/plexos-work/usr.erofs"
FSCK="${OUTPUT}/host/bin/fsck.erofs"
[ -r "${USR_EROFS}" ] || die "no /usr image at ${USR_EROFS}; the build did not finish"
[ -x "${FSCK}" ]      || die "no fsck.erofs at ${FSCK}; cannot read the image back"

WORK="$(mktemp -d)" || die "could not make a scratch directory"
trap 'rm -rf "${WORK}"' EXIT
"${FSCK}" --extract="${WORK}/usr" --overwrite "${USR_EROFS}" >/dev/null 2>&1 \
    || die "could not extract ${USR_EROFS}"
IMAGE_OSREL="${WORK}/usr/lib/os-release"
[ -r "${IMAGE_OSREL}" ] || die "the /usr image has no os-release in it"

field() { sed -n "s/^$1=//p" "${IMAGE_OSREL}" | tr -d '"'; }
VERSION="$(field VERSION_ID)"
BUILT_FROM="$(field MEDIALITH_COMMIT)"
BUILT_STATE="$(field MEDIALITH_SOURCE_STATE)"
ROOT_HASH="$(sed -n 's/.*"root_hash"[[:space:]]*:[[:space:]]*"\([0-9a-f]*\)".*/\1/p' "${UPDATE}" | head -1)"
[ -n "${VERSION}" ] || die "the image carries no VERSION_ID"
[ -n "${ROOT_HASH}" ] || ROOT_HASH="unknown"

COMMIT="$(git rev-parse HEAD 2>/dev/null)" || die "not a git repository"
TREE_DIRTY=0
[ -n "$(git status --porcelain 2>/dev/null)" ] && TREE_DIRTY=1

# -------------------------------------------------------------------- and does it agree?
DIRTY=""
provenance_failed() {
    if [ "${ALLOW_DIRTY}" != "1" ]; then die "$1"; fi
    DIRTY=" (UNREPRODUCIBLE: $2)"
    say "WARNING: $2."
    say "         --allow-dirty was given, so the notes say so. Do not publish this."
}

if [ -z "${BUILT_FROM}" ]; then
    provenance_failed "this image records no source commit, so nothing can prove what built it.
       It predates MEDIALITH_COMMIT in os-release. Remedy: rebuild.
         make -C ../buildroot-upstream O=\$(pwd)/${OUTPUT} all" \
      "the image records no source commit"
elif [ "${BUILT_STATE}" != "clean" ]; then
    provenance_failed "this image was built from a working tree with uncommitted changes
       (MEDIALITH_SOURCE_STATE=${BUILT_STATE}), so the source that produced it is not in
       history. Remedy: commit, rebuild, then package." \
      "the image was built from an uncommitted tree"
elif [ "${BUILT_FROM}" != "${COMMIT}" ]; then
    provenance_failed "the image and the checkout disagree about the source.
         the image was built from  ${BUILT_FROM}
         HEAD is                   ${COMMIT}
       This is exactly the case a clean git status cannot see: a stale output tree packaged
       against a newer commit. Remedy: rebuild from this commit, or check out the one the
       image names." \
      "the image was built from ${BUILT_FROM}, not from HEAD"
elif [ "${TREE_DIRTY}" = "1" ]; then
    provenance_failed "the working tree has uncommitted changes. The image matches HEAD, but
       the tree does not, so the notes could not honestly name a source anybody can check
       out. Remedy: commit or stash." \
      "the working tree has uncommitted changes"
fi

# `0.1.0.202608142020` -> `0.1.0`. The public name carries the product version; the build
# stamp is what identifies the artefact and it goes in the notes and the checksums.
OS_VERSION="${VERSION%.*}"
ASSET="MediaLith-${OS_VERSION}-technical-preview-${PREVIEW}-x86_64.img.xz"

mkdir -p "${DEST}" || die "could not create ${DEST}"

say "MediaLith ${OS_VERSION} Technical Preview ${PREVIEW}"
say "  build      ${VERSION}"
say "  built from ${BUILT_FROM:-<not recorded>} (${BUILT_STATE:-unknown})"
say "  HEAD       ${COMMIT}${DIRTY}"
say "  root hash  ${ROOT_HASH}"

# ------------------------------------------------------------------------------ compress
say "compressing (8 GiB of mostly zeros; a minute or so with every core)"
if ! xz -T0 -9 -c "${IMAGE}" > "${DEST}/${ASSET}"; then
    rm -f "${DEST}/${ASSET}"
    die "xz failed"
fi
SIZE_MB="$(du -m "${DEST}/${ASSET}" | cut -f1)"
say "  ${ASSET} -- ${SIZE_MB} MB"

# ----------------------------------------------------------------------------- checksums
# Over the compressed asset, because that is the file a person downloads. The build stamp
# is written beside it as a comment: `sha256sum -c` ignores anything after a `#`, and a
# checksum file that cannot say which build it belongs to is a checksum for an unknown
# thing.
{
    printf '# MediaLith %s Technical Preview %s\n' "${OS_VERSION}" "${PREVIEW}"
    printf '# build %s, commit %s\n' "${VERSION}" "${COMMIT}"
    printf '# verify with:  sha256sum -c SHA256SUMS\n'
    ( cd "${DEST}" && sha256sum "${ASSET}" )
} > "${DEST}/SHA256SUMS"
say "  SHA256SUMS"

# --------------------------------------------------------------------------------- notes
cat > "${DEST}/RELEASE-NOTES.md" <<NOTES
# MediaLith ${OS_VERSION} — Technical Preview ${PREVIEW}

| | |
| --- | --- |
| Image | \`${VERSION}\` |
| Built from | \`${BUILT_FROM:-not recorded}\`${DIRTY} — recorded inside the image, not taken from the checkout |
| \`/usr\` root hash | \`${ROOT_HASH}\` |
| Download | ${SIZE_MB} MB compressed, 8 GiB written |

## Read this before writing the image

- ⚠️ **Writing this image erases the target device completely**, including its partition
  table. Check the device name twice. On Linux it is the whole disk (\`/dev/sdb\`), never a
  partition (\`/dev/sdb1\`) — the image carries its own partition table.
- **UEFI only.** There is no legacy BIOS support.
- **Secure Boot must be turned off.** Kernel images are self-signed and no keys are
  enrolled (ADR-0004). This is separate from update signing, which is done.
- Nothing is written to the computer's own disks unless you ask for it. MediaLith runs
  from the stick until you tell it to install itself.

## Getting it onto a stick

    xz -d ${ASSET}
    sudo dd if=MediaLith-*.img of=/dev/sdX bs=4M status=progress conv=fsync

On Windows or macOS, use Rufus or Balena Etcher and pick the decompressed \`.img\`.

Verify the download first:

    sha256sum -c SHA256SUMS

## Requirements

- **Processor**: any 64-bit x86. Nothing above the architectural baseline is used —
  no SSE3, no SSE4, no POPCNT, no AVX.
- **Platform**: UEFI x86-64, GPT, and a disk and network adapter this kernel has a driver
  for. Everything on the boot path — storage, network and filesystem drivers — is built
  into the kernel rather than loaded, so a machine whose disk or NIC is not in that set
  will not come up. Loadable modules do exist and are used where they have to be (the
  NVIDIA driver), and the kernel enforces signatures on them.
- **Hardware transcoding** is a separate question again, and MediaLith answers it per
  machine on the Overview.

## Known limitations

- **No way to write to the appliance over the network.** No SSH, no Samba, no NFS server;
  every library is mounted read-only by design. Fill a drive on another computer and
  attach it.
- **It can read an NTFS filesystem and cannot create one.**
- **No AMD graphics.** Intel Arc is built and has never been tried — no hardware.
- **No update service exists yet.** The appliance ships with no address to check and never
  looks for one by itself; updates are applied from a bundle by hand.
- **Nothing writes \`/var/log\`**, and nothing prunes \`/var\`'s largest writer.
- **The pairing QR code needs a screen with room for it.** At 1280×800 it does not fit,
  and the console says so and falls back to the recovery code it prints beside it.
- **The management console is for a trusted LAN.** It offers a root shell and can replace
  the operating system. It is served over TLS with a self-signed certificate and every
  mutating request needs a credential — which stops anyone listening, and proves nothing
  against an active middle until somebody compares the fingerprint. Do not expose it to
  the internet.

## If a film will not start in a browser

Try a different browser, or a native Plex app. Some browsers advertise HEVC support they do
not have; Plex believes the client, sends the video untouched instead of converting it, and
nothing plays. It is a client-side misreport rather than anything on the appliance, and it
does not happen to H.264 files. Seen on one desktop browser while another on the same
machine, and the same browser on a different operating system, both played the same file.

## Not verified

Stated because a preview that only lists what works is not a preview.

- **No processor older than a Core i5-8265U has run this in silicon.** The baseline was
  verified on emulated CPU models under QEMU/TCG, which is good evidence about instruction
  sets and none at all about firmware, chipsets or errata.
- **Secure Boot has never been enrolled and enforcing.** Until it is, the kernel command
  line is editable by anyone holding the machine.
- **The root signing key is a development key.** Its private half sits on a build host, and
  every place that reports a signature says so.
- **No security review of any kind has been done.**

## Hardware seen working

| Machine | State |
| --- | --- |
| Core i5-8265U / UHD 620 | Boots, installs, provisions Plex, transcodes 4K HDR on the iGPU |
| Alder Lake-P laptop | Transcodes on the iGPU |
| RTX 5060 desktop, no integrated graphics | Transcodes through NVDEC/NVENC |
| Intel Arc | Built, never tried |
| AMD | Not built |

## Reporting something

Say which build you ran — \`${VERSION}\` — and what the Overview and System views showed.
Those two carry the slot, the root hash, the health checks and the whole kernel command
line, which is usually enough to tell a bad flash from a bad release.
NOTES
say "  RELEASE-NOTES.md"

cat <<DONE

>>> Nothing has been published.

    ${DEST}/
      ${ASSET}
      SHA256SUMS
      RELEASE-NOTES.md

    Check the notes, then create the release by hand:

      gh release create v${OS_VERSION}-technical-preview-${PREVIEW} \\
          --title "MediaLith ${OS_VERSION} Technical Preview ${PREVIEW}" \\
          --notes-file ${DEST}/RELEASE-NOTES.md \\
          ${DEST}/${ASSET} \\
          ${DEST}/SHA256SUMS \\
          ${DEST}/RELEASE-NOTES.md

    RELEASE-NOTES.md is both: --notes-file makes it the release body, and listing it
    again uploads it as an asset, so it travels with the image when somebody downloads
    the pair rather than reading the page.

DONE
