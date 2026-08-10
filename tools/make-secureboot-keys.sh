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

# The EFI signature lists, when the tools for them are present. Not fatal if they are
# not: the .cer files are enough for every firmware that offers "enrol key from file",
# and efitools is not installed on most build hosts.
if command -v cert-to-efi-sig-list >/dev/null && command -v sign-efi-sig-list >/dev/null; then
  for key in PK KEK db; do
    cert-to-efi-sig-list -g "$GUID" "${key}.crt" "${key}.esl"
  done
  # Each list signed by the level above it; PK signs itself, which is what UEFI expects
  # of the root of its own hierarchy.
  sign-efi-sig-list -g "$GUID" -k PK.key  -c PK.crt  PK  PK.esl  PK.auth
  sign-efi-sig-list -g "$GUID" -k PK.key  -c PK.crt  KEK KEK.esl KEK.auth
  sign-efi-sig-list -g "$GUID" -k KEK.key -c KEK.crt db  db.esl  db.auth
  chmod 644 ./*.esl ./*.auth
  echo "wrote .esl and .auth as well (efitools present)"
else
  echo "efitools not installed, so no .esl/.auth were written"
  echo "  This is fine for enrolling by hand from the firmware's own file browser."
  echo "  Install efitools if you want sbkeysync or KeyTool to do it instead."
fi

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

Then enrol db.cer in the machine's firmware, once, by hand. Until that is done the
signed image will NOT boot with Secure Boot on -- the signature is present and the
firmware has never heard of whoever made it. docs/DEVELOPMENT.md has the steps.

Back these up. A machine that has enrolled this db and lost the key cannot be given a
new signed image without going back into firmware setup.
EOF
