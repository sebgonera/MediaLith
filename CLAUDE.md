# PlexOS — working notes

An immutable, atomically-updated Linux appliance distribution built to run Plex Media
Server well. Read `docs/ARCHITECTURE.md` first, then `docs/adr/` for why anything is
the way it is.

## The one rule that shapes everything

Some artefacts reach a user's disk and can never be changed afterwards: partition
GUIDs and sizes, the update manifest schema, the config schema, the `/var` layout.
They live in `crates/plexos-types` and are covered by tests that fail if the wire
format shifts. **Changing anything there is a migration problem, not an edit.** Treat
that crate as append-only unless there is a very good reason.

Everything else is cheap to revise. Prefer revising it.

## Conventions

- **Verify, don't recall.** Buildroot option names, kernel `CONFIG_*` symbols, and PCI
  IDs get checked against the actual tree or an actual capture. Guessing here has
  already cost real bugs — see the "silently dropped options" commit.
- **Report what is unverified.** Several files in this repo have never been built or
  executed and say so at the top. Keep those notices accurate; delete them only when
  the thing has actually run.
- **Every diagnostic names a remedy.** `plexos-gpu` enforces this with a test. A report
  that says "hardware acceleration unavailable" and stops has reproduced the problem
  the project exists to fix.
- Repo content is English. Conversation may be Polish.
- `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
  before every commit. Lints are `pedantic`.
- **`unsafe_code` is forbidden everywhere except `crates/plexos-sys`**, which exists so
  that it can be. PID 1 has to issue syscalls; confining them to one small crate keeps
  the unsafe reviewable. Every block there carries a soundness comment, enforced by
  `clippy::undocumented_unsafe_blocks`. If you find yourself wanting `unsafe` in
  another crate, the answer is a function in `plexos-sys`, not an exception.

## Where things stand

| Component | State |
| --- | --- |
| `crates/plexos-types` | Done. Formats and the layout emitter, 41 tests. |
| `crates/plexos-gpu` | Done as a diagnostic tool, 41 tests. Never run against a real GPU. |
| `crates/plexos-sys` | The audited unsafe surface: verity superblock, dm ioctls, mount. 39 tests. **The ioctls and mounts have never been executed** — they need root. |
| `crates/plexos-init` | Plans and executes. 43 tests. Never run as PID 1. |
| `buildroot/` | defconfig, kernel fragment, systemd-boot, plexos-init package. **Build in progress; never completed.** |
| `post-image.sh` | Written. Stages 1, 2 and 6 verified against real tools; 3, 4, 5 await the build. |
| Installer, `plexosd`, Plex provisioning | Not started. |

Next: finish the first Buildroot build, assemble an image, and boot it under QEMU —
which is the first time any of the syscall code will have run at all. Then `plexosd`
and the health gate.

Nothing in `plexos-sys` or `plexos-init::execute` has executed a single syscall yet.
Unprivileged user namespaces are blocked by AppArmor on the development machine, so
QEMU is the first real test, not a formality.

## Known traps

- **Buildroot's `BR2_PACKAGE_SYSTEMD_BOOT` is unusable here** — it sits inside
  `if BR2_PACKAGE_SYSTEMD`, which only `BR2_INIT_SYSTEMD` selects. Hence
  `buildroot/package/systemd-boot/`.
- **kconfig drops options with unmet dependencies without erroring.** After editing
  the defconfig, always re-run kconfig and check the options actually survived. Four
  were being dropped at one point, and the result was a uClibc toolchain that Plex
  cannot run on.
- **Rollback reverts `/usr`, never `/var`.** Any migration must leave state the
  previous release can still read. `crates/plexos-init/src/state.rs` encodes this.
- **Tests that only compare a thing to itself will pass while it is wrong.** Every
  test in `plexos-types::partition` passed against two GUIDs that were not the ones
  the specification defines: they were well-formed, unique, correctly paired, and
  accepted by `sfdisk`. Nothing compared them to anything outside the crate. Where a
  constant comes from somewhere else — a spec, a kernel header — pin it against a
  value extracted from that source, and say in the test that the code is what changes
  when it fails.
- **`/tmp` is small, and GCC uses it.** A Buildroot build dies partway through
  `host-gcc-initial` with `Disk quota exceeded` if `/tmp` is a modest tmpfs. The
  message names the compiler, not `/tmp`. Set `TMPDIR` alongside the output directory.
- **The boot health gate must check Plex on loopback only.** USB Ethernet enumerates
  seconds after PCI; a gate that waited for the network would roll back good updates.

## Reference hardware

`tools/captures/huawei-wrt-wx9.txt` — Core i5-8265U (Whiskey Lake-U, Gen9.5), UHD
Graphics 620 `[8086:3ea0]`, NVMe, UEFI, Secure Boot enabled, no TPM, no built-in
Ethernet (USB adapter). This is the machine the defconfig targets and the one a first
image gets tried on.

Secure Boot must be turned off in firmware for testing, since images are self-signed.

## Building

See `docs/DEVELOPMENT.md`. Short version: Buildroot needs a Linux host with real
network access. It cannot be built in the hosted Claude Code environment — the proxy
there reaches GitHub and nothing else, and Buildroot fetches from a dozen other hosts.

## Open decisions, none blocking

1. **Name.** "PlexOS" uses a third-party trademark and likely needs to change. Cheap
   now, a state migration later — `/var/lib/plexos` is in the frozen layout.
2. **Secure Boot keys.** Enrol our own, or go through Microsoft's shim process.
3. **Licence.** Not chosen.
