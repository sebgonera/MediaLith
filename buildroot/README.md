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

A defconfig and kernel fragment exist and every option in them was checked against a
Buildroot checkout rather than written from memory. **Nothing here has been built.** A
Buildroot build needs hours and a large amount of disk, neither of which the
development environment has, so treat both files as a reviewed first draft that has
never been compiled.

One known blocker, documented in the defconfig: there is no bootloader yet. See below.

## What comes next, in order

1. **`package/systemd-boot-standalone/`** — the blocker. ADR-0005 chose `systemd-boot`
   used as a plain EFI application, but Buildroot's `BR2_PACKAGE_SYSTEMD_BOOT` sits
   inside `if BR2_PACKAGE_SYSTEMD`, and that is only ever selected by the
   `BR2_INIT_SYSTEMD` choice — so taking it would drag systemd in as PID 1. The
   decision in ADR-0005 is still right; its packaging simply does not follow for free.
   A package here can build just the bootloader from systemd's source, and `gnu-efi`
   is already available.
2. **`package/plexos-init/`** — the first Rust package. Buildroot's `pkg-cargo`
   infrastructure handles vendoring and cross-compilation.
3. **`board/plexos/x86_64/post-image.sh`** — assemble the image: build the erofs,
   compute the verity tree, embed the root hash in the UKI command line, sign the UKI,
   lay out the GPT per `plexos-types::partition::LAYOUT_X86_64`. The ordering is forced
   by ADR-0004 and the script must enforce it. A UKI is the EFI stub with `.osrel`,
   `.cmdline`, `.linux`, and `.initrd` sections added, so `objcopy` is enough —
   systemd's `ukify` would need `python-pefile`, which Buildroot does not carry.
4. **First boot**, on the reference machine rather than under QEMU. QEMU proves the
   boot path, verity, and the partition layout; it proves nothing at all about
   QuickSync, since virtio-gpu has no VA-API.

## Reference hardware

`tools/captures/huawei-wrt-wx9.txt` — the machine the defconfig targets. Core i5-8265U
(Whiskey Lake-U, Gen9.5), UHD Graphics 620 `[8086:3ea0]`, Wireless-AC 9560, NVMe, UEFI
with Secure Boot enabled, no TPM, and **no built-in Ethernet**. That last one has a
design consequence noted at the end of `linux.fragment`.

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
