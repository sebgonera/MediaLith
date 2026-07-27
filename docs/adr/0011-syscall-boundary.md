# ADR-0011: One crate for unsafe, and no libraries on the boot path

**Status:** Accepted
**Date:** 2026-07-27

## Context

`plexos-init` is PID 1. Its job is to create a device-mapper verity target, mount
filesystems, move mounts, and replace the root — and every one of those is a syscall.
Rust cannot express a syscall in safe code.

The workspace forbids `unsafe_code` everywhere. That rule was easy to keep while
`plexos-init` only *computed* a boot plan; it becomes a real question the moment
anything performs one.

There is a second, less obvious problem. `veritysetup` is the normal way to establish
dm-verity, and it lives in `cryptsetup`, which is installed into the target `/usr` —
behind the very device it would be needed to create. `udev` is the normal way for
`/dev/mapper/<name>` to appear, and the initrd deliberately has no `udev`. The usual
answers are unavailable here for structural reasons, not incidental ones.

## Decision

**All unsafe code lives in one crate, `plexos-sys`.** Every other crate keeps
`unsafe_code = "forbid"`. `plexos-sys` sets `unsafe_code = "allow"` and, in exchange,
accepts constraints the others do not need:

- `clippy::undocumented_unsafe_blocks` is denied, so every block carries a comment
  arguing its soundness.
- `clippy::multiple_unsafe_ops_per_block` is denied, so an argument cannot silently
  come to cover more than it was written for.
- Anything expressible safely is written safely and tested as such. The verity
  superblock parser is byte handling with no unsafe at all, and lives here only
  because it is the natural companion to the ioctls.

**`plexos-init` therefore talks to the kernel directly.** It issues the
device-mapper ioctls itself, reads the verity superblock itself, and creates
`/dev/mapper/<name>` itself, because `udev` is not there to do it.

**The only dependency is `libc`,** and it contributes declarations rather than code:
the symbols it names are in the libc `std` already links, so a `+crt-static` build
gains nothing that was not already present.

**ABI constants are extracted, never recalled.** Structure sizes, field offsets,
ioctl request numbers and `MS_*` flags were obtained by compiling a C program against
the system headers and printing `sizeof`, `offsetof` and the macro values. Tests pin
the results against those printed numbers.

## Alternatives considered

**Ship `veritysetup` in the initrd.** No unsafe anywhere, and the tool is known to
work. Rejected: it drags `libcryptsetup`, `libdevmapper`, `libudev` and their
dependencies onto the boot path, and the initrd stops being the single static binary
ARCHITECTURE.md §3 describes. The trade is a few hundred lines of reviewed unsafe
against several megabytes of C with its own CVE stream, on the one path where a
failure means a machine that does not come back.

**Use `rustix` or `nix`.** Either provides safe `mount`, `chroot` and `chdir`
wrappers, which would have cut the unsafe here to the ioctls alone — genuinely
attractive. Rejected because the syscalls in question are thin enough that wrapping
them costs less than carrying a general-purpose crate through every future audit of
the boot path, and because neither wraps the device-mapper ioctls, so unsafe would
remain regardless. This is the alternative most likely to be worth revisiting.

**Allow `unsafe` in `plexos-init` behind a narrow module.** Simpler on paper.
Rejected: `forbid` cannot be relaxed within a crate, so this means dropping the lint
for the crate that runs as PID 1 — the one place it is most worth having. A separate
crate keeps the boundary mechanical rather than a matter of discipline.

**Put the verity parameters on the kernel command line** instead of reading the
superblock, avoiding the parser. Rejected: it lets the UKI and the hash partition
disagree, and a mismatch surfaces as an I/O error naming nothing. The superblock is
already the authoritative record of how the tree was built.

## Consequences

- The unsafe surface of the whole system is auditable by reading one crate. At the
  time of writing that is two `unsafe` blocks for the ioctls and four for the mount
  and root-switching syscalls.
- `plexos-sys` is coupled to Linux ABI details that the compiler cannot check. A
  wrong offset yields `EINVAL` naming no field, so the extraction procedure above is
  part of the decision, not a convenience.
- The name is `plexos-sys` rather than `plexos-dm`, because `mount(2)`, `chroot(2)`
  and `execve(2)` are exactly as unsafe as the ioctls and splitting them across two
  crates would double the audit surface for nothing.
- The kernel must have device-mapper and dm-verity built in, not modular. Already
  required by ADR-0004 and enforced in `board/plexos/x86_64/linux.fragment`.
- If `plexosd` or the updater later need syscalls, they go here too. Growth is
  expected; what must not happen is a second crate that allows unsafe.
- **None of this code has executed.** The buffer construction, option parsing and
  superblock handling are tested, but issuing an ioctl needs root and a device-mapper
  device, and mounting needs privileges that unprivileged user namespaces do not
  grant on the development machine. QEMU is the first real test.
