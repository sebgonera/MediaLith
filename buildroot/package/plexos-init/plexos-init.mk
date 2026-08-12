################################################################################
#
# plexos-init
#
# PID 1, and the single file the initrd consists of.
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
#  1. Buildroot 2026.02 carries rustc 1.88.0, while rust-toolchain.toml pins 1.94 and
#     Cargo.toml declares rust-version = "1.94". cargo refuses outright on a version
#     mismatch. (The code does in fact compile and pass its tests on 1.88 -- that was
#     checked, not assumed -- so this is a declared floor rather than a real need.)
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

PLEXOS_INIT_VERSION = 0.1.0
PLEXOS_INIT_SITE = $(BR2_EXTERNAL_PLEXOS_PATH)/..
PLEXOS_INIT_SITE_METHOD = local

# Not yet chosen (CLAUDE.md, open decision 3). Stated honestly rather than guessed:
# legal-info output that claims a licence nobody picked is worse than none.
PLEXOS_INIT_LICENSE = NOT CHOSEN

# The whole repository is rsynced, so keep the build output and history out of it.
# target/ alone is comfortably larger than the rest of the tree put together, and it
# would be copied on every single build.
PLEXOS_INIT_OVERRIDE_SRCDIR_RSYNC_EXCLUSIONS = \
	--exclude=target \
	--exclude=output \
	--exclude=output-* \
	--exclude=.git

# The workspace's own target triple, not Buildroot's x86_64-buildroot-linux-gnu.
# Nothing from the Buildroot sysroot is linked in, so the two never meet.
PLEXOS_INIT_RUST_TARGET = x86_64-unknown-linux-gnu

# +crt-static is the entire reason this package looks the way it does. See Config.in.
PLEXOS_INIT_CARGO_ENV = \
	CARGO_TARGET_DIR=$(@D)/target \
	RUSTFLAGS="-C target-feature=+crt-static"

# rustup's shims are commonly installed without touching PATH, so looking only at
# PATH finds nothing on an otherwise perfectly good machine.
PLEXOS_INIT_CARGO = $(firstword \
	$(shell command -v cargo 2>/dev/null) \
	$(wildcard $(HOME)/.cargo/bin/cargo))

define PLEXOS_INIT_BUILD_CMDS
	@if [ -z "$(PLEXOS_INIT_CARGO)" ]; then \
		echo "plexos-init: no cargo found on PATH or in \$$HOME/.cargo/bin"; \
		echo "  remedy: install the Rust toolchain (docs/DEVELOPMENT.md), or"; \
		echo "          set BR2_PACKAGE_PLEXOS_INIT=n to build an image without PID 1"; \
		exit 1; \
	fi
	cd $(@D) && $(PLEXOS_INIT_CARGO_ENV) \
		$(PLEXOS_INIT_CARGO) build \
			--release \
			--locked \
			--package plexos-init \
			--target $(PLEXOS_INIT_RUST_TARGET)
endef

PLEXOS_INIT_BINARY = \
	$(@D)/target/$(PLEXOS_INIT_RUST_TARGET)/release/plexos-init

define PLEXOS_INIT_INSTALL_TARGET_CMDS
	# Checked here rather than left to post-image.sh. A dynamic binary is a
	# perfectly good executable that fails only at boot, several build stages
	# later, with a message about a missing loader that says nothing about why.
	#
	# The test is for a PT_INTERP program header, which names the dynamic loader
	# and is present only on dynamic executables. Matching file(1) output instead
	# is what one reaches for first and it is wrong: +crt-static produces a
	# *static-pie* binary, which file describes as "static-pie linked", so a grep
	# for "statically linked" rejects exactly the binary we want.
	@if readelf -l $(PLEXOS_INIT_BINARY) 2>/dev/null | grep -q INTERP; then \
		echo "plexos-init: built binary is dynamically linked"; \
		echo "  remedy: check RUSTFLAGS reached cargo; the initrd has no loader or libc"; \
		file -b $(PLEXOS_INIT_BINARY); \
		exit 1; \
	fi
	$(INSTALL) -D -m 0755 $(PLEXOS_INIT_BINARY) \
		$(TARGET_DIR)/usr/bin/plexos-init
endef

$(eval $(generic-package))
