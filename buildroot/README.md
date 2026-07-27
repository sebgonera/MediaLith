# PlexOS BR2_EXTERNAL tree

Buildroot's mechanism for keeping a downstream project outside the upstream tree.
Upstream Buildroot stays a pinned, unmodified checkout; everything PlexOS-specific
lives here (ADR-0002).

```
external.desc                 Tree name (PLEXOS) and description
external.mk                   Includes every package/*/*.mk
Config.in                     Sources package menu entries
configs/                      defconfigs, one per target
package/                      PlexOS packages (plexos-init, plexosd, ...)
board/plexos/x86_64/          Kernel config, post-image image assembly
```

Once a defconfig exists, builds run as:

```
make -C ../buildroot-upstream \
     BR2_EXTERNAL=$(pwd) \
     O=$(pwd)/output \
     plexos_x86_64_defconfig
```

## Status

Skeleton only. Nothing here builds yet — the directories exist so that the layout is
fixed before packages start landing in ad-hoc places.

## What comes next, in order

1. **`configs/plexos_x86_64_defconfig`** — minimal glibc x86-64 target, no BusyBox
   init (`plexos-init` is PID 1), erofs rootfs.
2. **`board/plexos/x86_64/linux.config`** — trimmed kernel. Storage, erofs, and
   dm-verity built in, not modular: there is no initramfs to load modules from
   (ADR-0004). Plus i915/xe, and GuC/HuC firmware.
3. **`package/plexos-init/`** — the first Rust package. Buildroot's
   `pkg-cargo` infrastructure handles vendoring and cross-compilation.
4. **`board/plexos/x86_64/post-image.sh`** — assemble the image: build the erofs,
   compute the verity tree, embed the root hash in the UKI command line, sign the UKI,
   lay out the GPT per `plexos-types::partition::LAYOUT_X86_64`. The ordering is
   forced by ADR-0004 and the script must enforce it.

## Rules for this tree

- Nothing enters the base image unless Plex needs it to run. The sub-100-package
  target is what makes CVE maintenance survivable, and it only holds if it is defended
  package by package.
- The upstream Buildroot version is pinned and bumped deliberately — it carries
  package versions and security patches, so "just track master" is not an option.
- `make pkg-stats` produces a CVE report over the package set. It goes into CI as soon
  as there is a defconfig to run it against.
- No partition GUID, size, or label is written here by hand. They come from
  `plexos-types::partition`, which is the single definition (ADR-0003).
