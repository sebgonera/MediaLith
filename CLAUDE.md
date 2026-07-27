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
| `crates/plexos-sys` | The kernel-interface layer, and the only crate allowed `unsafe`: verity superblock, dm ioctls, mount, exec, partition-label lookup. Every syscall in it has run on real hardware. |
| `crates/plexos-init` | Plans and executes the boot, and runs as PID 1 in both roles. The supervisor role runs the health gate, spawns the status console, and then starts a shell. 50 tests. |
| `crates/plexosd` | Health gate, boot-counter clearing, and the status console (ADR-0012): wired-network bring-up, a hand-written HTTP server, and a read-only page. 82 tests, including the device token of ADR-0013. **Working on the reference laptop:** the appliance brings up its own network, takes a DHCP lease, and serves the page to a browser on another machine. It took three boots and three faults to get there — bring-up ordering, `PATH`, and a missing `/tmp` — each hidden behind the one before it. |
| `crates/plexos-plex` | Provisioning Plex from its own signed packages (ADR-0010, ADR-0007): reads the `.deb`, verifies `_gpgplex` against a pinned key, ties it to the payload, builds an erofs app image, manages the version store. 64 tests. Runs end to end on the build host against real Plex downloads; never run on the appliance. |
| `buildroot/` | Builds. defconfig, kernel fragment, and packages for `plexos-init`, `plexosd`, `plexos-gpu` and `plexos-systemd-boot`. |
| `post-image.sh` | All six stages run, and produce an image that boots on hardware. |
| Installer, updater, first-boot wizard | Not started. |

**The image boots on the reference laptop, from a USB stick, to a shell.** tmpfs root,
`/usr` verified by dm-verity and mounted read-only, `/var` writable, `/etc` an overlay.
The health gate runs and `plexosd` clears the boot try counter, so a good slot becomes
permanent — confirmed by the entry on the ESP being renamed.

**This now works end to end on the reference laptop.** The appliance boots, verifies
`/usr`, passes the health gate, marks the slot good, brings up the USB Ethernet adapter,
takes a DHCP lease and serves its status console to a browser on another machine on the
LAN. Reading a diagnostic no longer means transcribing it off a 2160x1440 panel.

Getting there took three boots and three faults, all now in the trap list below and each
hidden behind the one before it: links brought up once before the wait rather than
during it, so the USB adapter that appears *during* the wait was never raised; `ip` and
`udhcpc` invoked by name from a process with no `PATH`; and no `/tmp` at all on the
assembled root, which broke udhcpc's lease script. The lesson worth keeping is that
every one of them passed a full test suite, because every test described a machine
where the thing being waited for had already happened.

Next, in order:

1. **Run `plexos-gpu` on the reference laptop.** It has 41 tests and has now seen
   exactly one real GPU — an NVIDIA GTX 1060 on the build host, which it correctly
   reported as unsupported. It has still never seen the UHD 620 it was written for.
   This is the question the project exists to answer and it is still open. The status
   console exists partly to make the answer readable when it arrives.
2. **Reach the provisioning code from the device.** `plexos-plex` does the whole job —
   verify, unpack, build, publish, activate — and nothing on the appliance can call it.
   What is missing is a downloader, an authenticated upload route (ADR-0013), a
   removable-media path (ADR-0010), and mounting the image once it exists (ADR-0007).
   Until Plex actually runs, the gate's `plex-http` check reports `NotApplicable`,
   which is correct and means the gate is weaker than ADR-0005 intends.
3. **`plexos-update`** — nothing implements the update flow, so rollback has never
   been exercised end to end.

Still unproven: hardware transcoding, updates, rollback, and signing. Images are
unsigned, so Secure Boot must be off.

## Known traps

- **Buildroot's `BR2_PACKAGE_SYSTEMD_BOOT` is unusable here** — it sits inside
  `if BR2_PACKAGE_SYSTEMD`, which only `BR2_INIT_SYSTEMD` selects. Hence
  `buildroot/package/plexos-systemd-boot/`.
- **A package's directory name becomes its kconfig symbol.** Name a package after an
  upstream one and you get a duplicate `BR2_PACKAGE_*` definition, which kconfig
  merges silently rather than rejecting. Prefix anything that collides with `plexos-`.
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
- **The last `console=` on the kernel command line becomes `/dev/console`.** Kernel
  messages go to every console listed; userspace output goes only to that one. Put
  `console=tty0` last, or every diagnostic disappears into a serial port the machine
  may not have. This cost three images to find.
- **QEMU cannot test the console path.** Under `-nographic` the console is a serial
  port, which is the one channel that is invisible on the reference laptop. Verify a
  boot by booting a *copy* of the image and checking that the ESP entry was renamed
  from `plexos-0.1.0+3.efi` to `plexos-0.1.0.efi` — on-disk evidence that does not
  depend on where output went.
- **`udev` does not exist here, and three separate things assumed it did.**
  `/dev/mapper/<name>`, `/dev/disk/by-partlabel/*` for the root, and the same for the
  ESP. `plexos_sys::device` resolves labels through sysfs `PARTNAME`; use it rather
  than opening a `by-partlabel` path.
- **The boot health gate must check Plex on loopback only.** USB Ethernet enumerates
  seconds after PCI; a gate that waited for the network would roll back good updates.
- **`carrier` is unreadable until the interface is up.** sysfs returns `EINVAL`, not
  `0`, on an administratively down interface. So "wait for a cable" cannot come first:
  every candidate has to be brought up before its carrier means anything, and code
  that treats the read failure as an error will abort enumeration on a machine whose
  link is merely not up yet — the normal state early in boot.
- **Bringing the links up once, before the wait, is the same as never.** The corollary
  of the above, and it cost a boot. The interface being waited for is the one that
  arrives late over USB, so at the moment of a single pre-loop pass it does not exist
  to be brought up. It then sits down for the entire timeout with an unreadable
  carrier, and the wait expires against an adapter that was plugged in throughout, on a
  live cable. The bring-up belongs **inside** the poll loop. `plexosd::net` has a
  regression test for it, which needs an `Environment` that enumerates the device late
  and refuses a carrier until something brings it up — the immutable `Fixture` cannot
  express either half, and a suite built only on fixtures passed while this was broken.
- **`operstate` cannot tell "nothing brought it up" from "no cable".** It reads `down`
  for both, and they take opposite remedies. Only `IFF_UP` in sysfs `flags` separates
  them. A diagnostic that reports `operstate` alone will send someone to check a cable
  that was never the problem.
- **The running root contains only what `plan.rs` puts there.** It is a tmpfs assembled
  from nothing, so directories present in the Buildroot rootfs — `/tmp` among them —
  never reach the booted system. `/tmp` was missing for the whole life of the project
  and nothing noticed until udhcpc's lease script called `mktemp`, which failed with
  `mktemp: : No such file or directory`: a message whose empty path names neither
  `/tmp` nor the script's intent. Anything the plan does not create does not exist.
- **There is no `PATH`, so run programs by absolute path.** PID 1 gets the environment
  the kernel provides, which is empty, and everything it spawns inherits that. glibc's
  `execvp` then falls back to `confstr(_CS_PATH)` — `/bin:/usr/bin`, confirmed with
  `getconf PATH` — while busybox installs `ip` and `udhcpc` into `/sbin` and
  `/usr/sbin` only. So `Command::new("ip")` fails from a daemon with a bare `ENOENT`
  while the same name typed at the shell works, because the shell sets its own `PATH`.
  `plexosd::net::resolve` searches the four directories explicitly. Beware verifying
  this on the build host: Ubuntu has `/bin/ip`, so an `env -i` test there succeeds and
  suggests, wrongly, that the fallback is enough.
- **Bridges and `veth` pairs are `ARPHRD_ETHER` and report a carrier.** Interface type
  alone cannot tell a network card from `docker0`, and virtual devices sort before the
  real one by name. Only hardware has a `device` symlink in sysfs; that is the
  discriminator. The appliance has no bridges today, which is exactly why this is worth
  encoding before something adds one.
- **A wrong remedy is worse than none.** `could not bind :80` first suggested "pass a
  higher port", which is right for `EACCES` and actively misleading for `EADDRINUSE`,
  where the port is fine and something else holds it. Match the remedy to the error
  kind, not to the operation that failed.

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
