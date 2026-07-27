################################################################################
#
# plexos-plex-keyring
#
# Plex Inc.'s OpenPGP public key, pinned into the image.
#
# ADR-0010 has the appliance fetch Plex Media Server from Plex's own servers at
# first boot and verify what arrives. That verification is worth nothing if the
# key it trusts arrives alongside the artefact, so the key ships here instead:
# inside /usr, which is mounted read-only and verified by dm-verity (ADR-0004).
# Replacing it therefore means replacing a signed OS image, which is exactly the
# ceremony a trust root should require.
#
# The file is the *dearmored* key. gpgv cannot read an ASCII-armored one: it
# prints "using RSA key ..." and then fails with "invalid packet (ctb=2d)",
# which reads like a damaged signature rather than the wrong keyring format.
#
################################################################################

PLEXOS_PLEX_KEYRING_VERSION = 1
PLEXOS_PLEX_KEYRING_SITE = $(BR2_EXTERNAL_PLEXOS_PATH)/package/plexos-plex-keyring
PLEXOS_PLEX_KEYRING_SITE_METHOD = local
PLEXOS_PLEX_KEYRING_LICENSE = Proprietary (Plex Inc. public key, redistributed as published)

# Where plexos-plex looks for it. Kept in step with PLEX_KEYRING in
# crates/plexos-plex/src/verify.rs, which has a test asserting this path.
PLEXOS_PLEX_KEYRING_TARGET = /usr/share/plexos/plex-signing-key.gpg

# The fingerprint every Plex package and repository index is signed with,
# confirmed against both of Plex's channels on 2026-07-27.
PLEXOS_PLEX_KEYRING_FPR = CD665CBA0E2F88B7373F7CB997203C7B3ADCA79D

# Checked at build time, not at boot. A keyring that is the wrong key produces a
# perfectly clear "Good signature from someone else" at provisioning time on a
# user's machine, months later; here it stops the build. The check needs gpg on
# the build host and is skipped with a loud notice if there is none, because
# failing the whole build over a missing host tool would be worse than building
# an image whose keyring was not re-confirmed this time.
define PLEXOS_PLEX_KEYRING_CHECK_FINGERPRINT
	@if command -v gpg >/dev/null 2>&1; then \
		found=$$(gpg --show-keys --with-colons $(@D)/plex-signing-key.gpg 2>/dev/null \
			| awk -F: '/^fpr/{print $$10; exit}'); \
		if [ "$$found" != "$(PLEXOS_PLEX_KEYRING_FPR)" ]; then \
			echo "plexos-plex-keyring: the keyring is not Plex's key"; \
			echo "  expected: $(PLEXOS_PLEX_KEYRING_FPR)"; \
			echo "  found:    $$found"; \
			echo "  remedy: re-fetch https://downloads.plex.tv/plex-keys/PlexSign.key,"; \
			echo "          run it through 'gpg --dearmor', and confirm the fingerprint"; \
			echo "          against a second source before trusting it."; \
			exit 1; \
		fi; \
		echo "plexos-plex-keyring: fingerprint $(PLEXOS_PLEX_KEYRING_FPR) confirmed"; \
	else \
		echo "plexos-plex-keyring: no gpg on the build host; keyring fingerprint NOT checked"; \
	fi
endef
PLEXOS_PLEX_KEYRING_PRE_INSTALL_TARGET_HOOKS += PLEXOS_PLEX_KEYRING_CHECK_FINGERPRINT

define PLEXOS_PLEX_KEYRING_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0444 $(@D)/plex-signing-key.gpg \
		$(TARGET_DIR)$(PLEXOS_PLEX_KEYRING_TARGET)
endef

$(eval $(generic-package))
