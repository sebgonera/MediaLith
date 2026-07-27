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
  before every commit. Lints are `pedantic`, `unsafe_code` is forbidden.

## Where things stand

| Component | State |
| --- | --- |
| `crates/plexos-types` | Done. Formats, 31 tests. |
| `crates/plexos-gpu` | Done as a diagnostic tool, 40 tests. Never run against a real GPU. |
| `crates/plexos-init` | Half. Computes the boot plan; does not execute it. |
| `buildroot/` | defconfig + kernel fragment + systemd-boot package. **Never built.** |
| `post-image.sh` | Missing. Last piece before a first build. |
| Installer, `plexosd`, Plex provisioning | Not started. |

Next, in order: `post-image.sh`, then the execution half of `plexos-init`, then
`package/plexos-init/` (pkg-cargo), then iterate on a real build.

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
