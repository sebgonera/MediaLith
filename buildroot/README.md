# MediaLith BR2_EXTERNAL tree

Buildroot's mechanism for keeping a downstream project outside the upstream tree.
Upstream Buildroot stays a pinned, unmodified checkout; everything MediaLith-specific
lives here (ADR-0002).

```
external.desc                 Tree name (PLEXOS) and description
external.mk                   Includes every package/*/*.mk
Config.in                     Sources package menu entries
configs/                      defconfigs, one per target
package/                      MediaLith packages (plexos-init, plexosd, ...)
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

The build completes and the image it produces boots to a shell under QEMU.

| | |
| --- | --- |
| `configs/plexos_x86_64_defconfig` | Every option verified to survive kconfig, twice. |
| `board/plexos/x86_64/linux.fragment` | Compiles, boots, and provides verity, erofs, XFS and overlayfs. |
| `package/plexos-systemd-boot/` | Builds the bootloader **and** `linuxx64.efi.stub`. Both are used. |
| `package/plexos-init/` | Builds under Buildroot, and the binary is what runs as PID 1. |
| `board/plexos/x86_64/post-image.sh` | All six stages run; the image boots. |

The upstream Buildroot version is pinned to **2026.02.3**. The `YYYY.02` series is the
one upstream maintains long-term, which is what makes the CVE story in ADR-0002
survivable. It carries systemd 258.7, matching `package/plexos-systemd-boot`, and rustc
1.88.0 — see `package/plexos-init/plexos-init.mk` for why that last number matters.

## What comes next, in order

1. **`plexosd` and the health gate** (ADR-0005). Nothing currently clears the boot try
   counter, so as soon as updates exist every one of them would eventually roll back.
2. **Plex provisioning** (ADR-0010) and app-image mounting (ADR-0007).
3. **First boot on the reference machine.** QEMU proves the boot path, verity, and the
   partition layout; it proves nothing at all about QuickSync, since virtio-gpu has no
   VA-API.

Note when testing under QEMU: pass `-cpu Nehalem` or better. The defconfig targets
corei7 and QEMU's default `qemu64` does not implement it, so the first Buildroot-built
binary to run dies with SIGILL.

## Two traps already paid for

**Package names become kconfig symbols, so ours are prefixed.** Buildroot derives a
package's enable symbol from its directory name. A package called `systemd-boot` would
therefore declare `BR2_PACKAGE_SYSTEMD_BOOT` — which upstream already defines in
`package/systemd/Config.in`, inside `if BR2_PACKAGE_SYSTEMD`, from a file sourced
unconditionally. kconfig merges duplicate definitions instead of erroring, and
upstream's `systemd.mk` separately assigns `SYSTEMD_BOOT_EFI_ARCH`.

Both collisions happened to be harmless: our prompt kept the symbol reachable, the
`select BR2_PACKAGE_SYSTEMD_EFI` was scoped to the `if` and never fired, and both
assignments computed `x64`. None of that was intended, and none of it was guaranteed
to keep holding. Hence `package/plexos-systemd-boot/`. Anything added here that shares
a name with an upstream package needs the same treatment.

**Upstream never installs the UKI stub.** A Unified Kernel Image *is*
`linuxx64.efi.stub` with sections appended, and Buildroot's systemd package installs
only the bootloader. Without the addition in `package/plexos-systemd-boot/`, there is nothing
to build an image around — the bootloader alone is not enough.

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
