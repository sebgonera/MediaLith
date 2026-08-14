################################################################################
#
# plexosd
#
# The management daemon, and the only thing permitted to declare a boot good.
#
# NOT YET BUILT under Buildroot. The build command below has been run by hand with
# the same toolchain, target and flags, and produces a 1.5 MiB static-pie binary that
# `ldd` reports as statically linked. What has not been exercised is Buildroot driving
# it. Delete this notice once it has.
#
# ---------------------------------------------------------------------------
# Why this is not a pkg-cargo package
# ---------------------------------------------------------------------------
#
# Every other package here is compiled by Buildroot. This one is compiled by the
# workspace's own pinned Rust toolchain, for three reasons that all point the same
# way:
#
#  1. Buildroot 2026.02 carries rustc 1.88.0, while rust-toolchain.toml pins 1.94.
#     See package/plexos-init/plexos-init.mk, which explains this at length; the same
#     reasoning applies unchanged.
#
#  2. The output is a *static* binary. It links no target library, so Buildroot's
#     cross toolchain contributes nothing to it. Handing the job to Buildroot would
#     buy a second compiler with no corresponding benefit.
#
#  3. pkg-cargo vendors dependencies during the download step, and a package with
#     BR2_EXTERNAL-local sources has no download step -- SITE_METHOD=local sets
#     OVERRIDE_SRCDIR, which rsyncs instead. Its `cargo build --offline` would have
#     nothing to work from without a separate vendoring hook.
#
# The cost is real and worth stating plainly: an image build now needs cargo on the
# build host, so Buildroot no longer builds everything itself. That is acceptable
# during bring-up because docs/DEVELOPMENT.md already requires a Rust toolchain to
# work on the workspace at all. Before a public image ships, this should become a
# proper pkg-cargo package with a vendored dependency set, at which point the Rust
# version question above has to be settled rather than sidestepped.
#
################################################################################

PLEXOSD_VERSION = 0.1.0
PLEXOSD_SITE = $(BR2_EXTERNAL_PLEXOS_PATH)/..
PLEXOSD_SITE_METHOD = local

# Not yet chosen (CLAUDE.md, open decision 3). Stated honestly rather than guessed:
# legal-info output that claims a licence nobody picked is worse than none.
PLEXOSD_LICENSE = NOT CHOSEN

# The whole repository is rsynced, so keep the build output and history out of it.
# target/ alone is comfortably larger than the rest of the tree put together, and it
# would be copied on every single build.
#
# The output patterns are root-anchored -- a leading / in an rsync filter means "at the
# transfer root", which is this repository. That matters in both directions. Unanchored,
# `output` would also exclude any nested directory that happens to be called that, which
# is a silent way to ship a package missing a directory nobody thought about. Anchored,
# it covers exactly the Buildroot output trees, which live at the root and only there.
#
# The invariant: no root-level MediaLith output tree may be copied into a package build
# directory. It is not decoration. The destination is *inside* the source, so a missing
# pattern is not a wasted copy, it is a recursion -- and a recursion does not fail, it
# fills the disk. `--exclude=output` alone was correct until this repository had a second
# output tree, at which point one sync took the disk from 20 GiB to 698 GiB with no error
# and no symptom beyond a build that appeared to sit on "Syncing from source dir".
#
# post-image-test.sh stage 9 tests this by running rsync, not by grepping for the
# patterns: the property that matters is what gets copied.
PLEXOSD_OVERRIDE_SRCDIR_RSYNC_EXCLUSIONS = \
	--exclude=target \
	--exclude=/output/ \
	--exclude=/output-*/ \
	--exclude=.git

# The workspace's own target triple, not Buildroot's x86_64-buildroot-linux-gnu.
# Nothing from the Buildroot sysroot is linked in, so the two never meet.
PLEXOSD_RUST_TARGET = x86_64-unknown-linux-gnu

# +crt-static is the entire reason this package looks the way it does. See Config.in.
PLEXOSD_CARGO_ENV = \
	CARGO_TARGET_DIR=$(@D)/target \
	RUSTFLAGS="-C target-feature=+crt-static"

# rustup's shims are commonly installed without touching PATH, so looking only at
# PATH finds nothing on an otherwise perfectly good machine.
PLEXOSD_CARGO = $(firstword \
	$(shell command -v cargo 2>/dev/null) \
	$(wildcard $(HOME)/.cargo/bin/cargo))

define PLEXOSD_BUILD_CMDS
	@if [ -z "$(PLEXOSD_CARGO)" ]; then \
		echo "plexosd: no cargo found on PATH or in \$$HOME/.cargo/bin"; \
		echo "  remedy: install the Rust toolchain (docs/DEVELOPMENT.md), or"; \
		echo "          set BR2_PACKAGE_PLEXOSD=n to build an image without PID 1"; \
		exit 1; \
	fi
	cd $(@D) && $(PLEXOSD_CARGO_ENV) \
		$(PLEXOSD_CARGO) build \
			--release \
			--locked \
			--package plexosd \
			--target $(PLEXOSD_RUST_TARGET)
endef

PLEXOSD_BINARY = \
	$(@D)/target/$(PLEXOSD_RUST_TARGET)/release/plexosd

define PLEXOSD_INSTALL_TARGET_CMDS
	# Checked here rather than left to post-image.sh. A dynamic binary is a
	# perfectly good executable that fails only at boot, several build stages
	# later, with a message about a missing loader that says nothing about why.
	#
	# The test is for a PT_INTERP program header, which names the dynamic loader
	# and is present only on dynamic executables. Matching file(1) output instead
	# is what one reaches for first and it is wrong: +crt-static produces a
	# *static-pie* binary, which file describes as "static-pie linked", so a grep
	# for "statically linked" rejects exactly the binary we want.
	@if readelf -l $(PLEXOSD_BINARY) 2>/dev/null | grep -q INTERP; then \
		echo "plexosd: built binary is dynamically linked"; \
		echo "  remedy: check RUSTFLAGS reached cargo; the initrd has no loader or libc"; \
		file -b $(PLEXOSD_BINARY); \
		exit 1; \
	fi
	$(INSTALL) -D -m 0755 $(PLEXOSD_BINARY) \
		$(TARGET_DIR)/usr/bin/plexosd
endef

$(eval $(generic-package))
