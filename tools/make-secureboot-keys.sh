#!/usr/bin/env bash
#
# Generate the three Secure Boot keys PlexOS signs with, once.
#
#   tools/make-secureboot-keys.sh [directory]     # default ~/.plexos-keys/secureboot
#
# ADR-0004 closed every link of the boot chain except the first: firmware verifies the
# bootloader, and nothing said whose key it verifies it against. This is that key, and the
# two above it.
#
# # The three, and why there are three
#
# UEFI's hierarchy is fixed and each key exists to sign the level below it:
#
#   PK  (Platform Key)      one per machine; owning it is what "owns" Secure Boot
#   KEK (Key Exchange Key)  signs updates to the signature databases
#   db  (Signature DB)      the keys firmware will actually launch a binary with
#
# Only **db** signs anything PlexOS ships. PK and KEK exist so the enrolment is a proper
# hierarchy rather than a db entry with nothing above it -- firmware that is put back into
# User Mode wants a PK, and a machine whose PK belongs to nobody cannot have its databases
# updated later without clearing them entirely.
#
# # What this deliberately does not do
#
# It does not touch any firmware. Enrolment is a physical act at the machine, by design:
# a script that could add a key to the platform database from userspace would be a
# considerably more interesting thing than anything it was protecting. See
# docs/DEVELOPMENT.md for what to press.
#
# It also does not put the keys in the repository, for the same reason ~/.plexos-keys is
# outside it. A signing key in a git history is a signing key that has been published.

set -euo pipefail

DIR="${1:-$HOME/.plexos-keys/secureboot}"

command -v openssl >/dev/null || { echo "openssl is not installed" >&2; exit 1; }
command -v uuidgen >/dev/null || { echo "uuidgen is not installed (util-linux)" >&2; exit 1; }

if [ -e "$DIR" ] && [ -n "$(ls -A "$DIR" 2>/dev/null)" ]; then
  echo "refusing to overwrite the keys already in $DIR" >&2
  echo "  Remedy: a machine that has enrolled these cannot boot anything signed by a" >&2
  echo "  replacement until its firmware is cleared, so a second set is generated" >&2
  echo "  somewhere else and enrolled deliberately, never by re-running this." >&2
  exit 1
fi

mkdir -p "$DIR"
chmod 700 "$DIR"
cd "$DIR"

# One GUID identifies this owner across all three databases, which is what lets a later
# enrolment replace *our* entries rather than everything the firmware holds.
GUID_FILE="GUID.txt"
uuidgen > "$GUID_FILE"
GUID=$(cat "$GUID_FILE")

# 20 years. These are enrolled by hand into firmware that frequently has no working clock
# and no way to be told a certificate expired, so a short life is a machine that stops
# booting for a reason nobody can diagnose from the outside.
DAYS=7300

for key in PK KEK db; do
  case "$key" in
    PK)  subject="PlexOS Platform Key" ;;
    KEK) subject="PlexOS Key Exchange Key" ;;
    db)  subject="PlexOS Signature Database Key" ;;
  esac

  openssl req -newkey rsa:2048 -nodes -keyout "${key}.key" \
    -new -x509 -sha256 -days "$DAYS" -subj "/CN=${subject}/" -out "${key}.crt" 2>/dev/null

  # Two encodings of the same certificate. Firmware setup screens vary: some read DER
  # (.cer), some want the EFI signature-list form (.esl/.auth) that sbkeysync and KeyTool
  # take. Producing them here means the answer to "my BIOS will not read this file" is a
  # different extension rather than another session with openssl.
  openssl x509 -outform DER -in "${key}.crt" -out "${key}.cer"

  chmod 600 "${key}.key"
  chmod 644 "${key}.crt" "${key}.cer"
done

# The EFI signature lists and the authenticated updates.
#
# These are not an optional extra, which is what an earlier version of this script assumed.
# It wrote the .crt and .cer and said, when efitools was absent, that this was "fine for
# enrolling by hand" -- and it is not. A `.cer` is a bare certificate; what firmware is
# asked to store in db is an **EFI signature list**, and what a authenticated write takes
# is a **signed variable update**. Plenty of firmware offers "enrol key from file" and
# then silently stores nothing when handed a format it does not parse, which is exactly
# what happened: a key that appeared to be enrolled, a Secure Boot toggle that appeared to
# be on, and a machine that booted with it off because the platform never left Setup Mode.
#
# So the tools are now obtained rather than hoped for, and their absence is fatal.
ensure_efitools() {
  command -v cert-to-efi-sig-list >/dev/null && command -v sign-efi-sig-list >/dev/null \
    && return 0

  # Debian and Ubuntu ship them in `efitools`, and neither downloading nor unpacking a
  # .deb needs root -- so a build host without the package is not a reason to produce a
  # partial set of keys.
  command -v apt-get >/dev/null && command -v dpkg-deb >/dev/null || return 1

  echo "efitools is not installed; fetching it without installing it..." >&2
  local unpack
  unpack=$(mktemp -d)
  (
    cd "$unpack" || exit 1
    apt-get download efitools >/dev/null 2>&1 || exit 1
    dpkg-deb -x ./efitools_*.deb . >/dev/null 2>&1 || exit 1
  ) || { rm -rf "$unpack"; return 1; }

  PATH="$unpack/usr/bin:$PATH"
  export PATH
  # Removed when the script exits, whichever way it exits.
  trap 'rm -rf "$unpack"' EXIT
  command -v cert-to-efi-sig-list >/dev/null && command -v sign-efi-sig-list >/dev/null
}

if ! ensure_efitools; then
  echo "cannot produce .esl/.auth: cert-to-efi-sig-list and sign-efi-sig-list are missing" >&2
  echo "  and could not be fetched. Remedy: apt install efitools, then run this again." >&2
  echo "  The .crt and .cer above are written and usable for signing, but a key enrolled" >&2
  echo "  from a bare .cer is one many firmwares accept and then do not store." >&2
  exit 1
fi

for key in PK KEK db; do
  cert-to-efi-sig-list -g "$GUID" "${key}.crt" "${key}.esl"
done

# Each list signed by the level above it; PK signs itself, which is what UEFI expects of
# the root of its own hierarchy.
sign-efi-sig-list -g "$GUID" -k PK.key  -c PK.crt  PK  PK.esl  PK.auth
sign-efi-sig-list -g "$GUID" -k PK.key  -c PK.crt  KEK KEK.esl KEK.auth
sign-efi-sig-list -g "$GUID" -k KEK.key -c KEK.crt db  db.esl  db.auth
chmod 644 ./*.esl ./*.auth

# Six files, or the set is not the set. Checked rather than assumed, because every tool
# above exits 0 having written nothing if its input was not what it expected.
for want in PK KEK db; do
  for ext in esl auth; do
    [ -s "${want}.${ext}" ] || { echo "${want}.${ext} was not written" >&2; exit 1; }
  done
done

cat <<EOF

Secure Boot keys written to $DIR
  owner GUID: $GUID

What signs what:
  db.key   signs the bootloader and both UKIs, at build time
  KEK.key  signs changes to db
  PK.key   signs changes to KEK, and is the root of the hierarchy

To build a signed image:
  export PLEXOS_SB_KEY=$DIR/db.key
  export PLEXOS_SB_CERT=$DIR/db.crt

To enrol, in this order, from the firmware's own setup screens:

  1. db.auth   the key that signs the bootloader and the UKIs
  2. KEK.auth  only once db is visibly listed in firmware
  3. PK.auth   LAST -- this is what takes the platform out of Setup Mode and turns
               enforcement ON. Until a PK is enrolled, Secure Boot is not enforced no
               matter what the setup screen's toggle says, and the kernel will report
               "Secure boot disabled" on a machine whose firmware claims it is enabled.

Prefer .auth; fall back to .esl. A bare .cer is what many firmwares accept and then do
not store, which looks identical to success until something checks. After each step,
confirm the key is *listed* in firmware before going on to the next.

Until db is really enrolled, a signed image boots exactly as an unsigned one did --
the signature is present and the firmware has never heard of whoever made it.
docs/DEVELOPMENT.md has the rest.

Back these up. A machine that has enrolled this db and lost the key cannot be given a
new signed image without going back into firmware setup.
EOF
