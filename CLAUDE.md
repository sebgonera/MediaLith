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
| `crates/plexos-gpu` | Done, 41 tests, and it has now answered the question it was written for. On the reference laptop: UHD 620, iHD 26.1.2, VA-API 1.23, GuC and HuC both running, verdict `ready`. |
| `crates/plexos-sys` | The kernel-interface layer, and the only crate allowed `unsafe`: verity superblock, dm ioctls, mount, exec/execve, partition labels, Landlock, privilege dropping, and `reboot(2)`. 71 tests. The boot syscalls have run on real hardware; Landlock is proven by `examples/landlock-demo` on a build host; privilege dropping has not run anywhere. |
| `crates/plexos-init` | Plans and executes the boot, and runs as PID 1 in both roles. The supervisor role runs the health gate, spawns the status console, and then starts a shell. 50 tests. |
| `crates/plexosd` | Health gate, boot-counter clearing, and the status console (ADR-0012): wired-network bring-up, a hand-written HTTP server, the page, the ADR-0013 device token and the gate that enforces it, mounting the Plex app image at boot, claiming the device at first start, provisioning Plex in the background, starting it confined, and stopping the machine cleanly from the page. 141 tests. **Working on the reference laptop:** the appliance brings up its own network, takes a DHCP lease, and serves the page to a browser on another machine. It took three boots and three faults to get there — bring-up ordering, `PATH`, and a missing `/tmp` — each hidden behind the one before it. |
| `crates/plexos-plex` | Provisioning Plex from its own signed packages (ADR-0010, ADR-0007): reads the `.deb`, verifies `_gpgplex` against a pinned key, ties it to the payload, builds an erofs app image, manages the version store, mounts it with the hash checked first, bounds it with cgroup v2, and holds the confine-then-exec sequence. 94 tests. Provisioning runs end to end on the build host against real Plex downloads; nothing here has run on the appliance. `plexosd::provision` is now its caller. |
| `buildroot/` | Builds. defconfig, kernel fragment, a users table for the `plex` account, and packages for `plexos-init`, `plexosd`, `plexos-gpu`, `plexos-systemd-boot` and `plexos-plex-keyring`. |
| `post-image.sh` | All stages run, and produce an image that boots on hardware. Stage 0 applies the users table, which Buildroot itself applies too late to reach `/usr`. 42 checks in `post-image-test.sh`, none skipped on a machine with the Buildroot tree. |
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

**A Plex a person can install from a browser is now written, and none of it has run
on the appliance.** The console claims the device at first start and prints the token,
`POST /api/provision` installs Plex in the background while `GET /api/provision`
reports progress, `plexosd::plex` starts it confined, and the page has a token field, a
button, a progress area and a link to Plex itself. What is unproven is everything: no
appliance has claimed itself, downloaded a package, or started Plex. That is the next
thing to do and it needs hardware, not code.

Next, in order:

1. **Prove the four above on the reference laptop**, in that order, because each one
   only becomes reachable once the one before it worked. The token banner appears on
   the attached screen; the browser on another machine does the rest.
2. **Upload from a local disk**, and the removable-media path of ADR-0010. Both were
   asked for and both are deferred: an 83 MB upload has to stream to disk, and
   `http::MAX_BODY` is deliberately 64 KiB so that route reads the socket itself.
3. **Run Plex through a real transcode**, which is the last thing standing between
   "QuickSync works" and "this appliance does its job".
4. **`plexos-update`** — nothing implements the update flow, so rollback has never been
   exercised end to end. It is the riskiest untested path in the project, because a bug
   there bricks a device rather than degrading it.
5. **A supervisor.** `plexos-init` still prints "no supervisor yet" and hands over to a
   shell. Nothing restarts a service that dies — and `plexosd::plex` says so rather than
   pretending: a Plex that exits stays exited, and a newly provisioned version does not
   replace a running one without a reboot.
6. **A terminal in the status console**, so administering the appliance stops meaning
   PuTTY on another machine. The pieces are mostly there — `plexosd` already owns an
   HTTP server and the ADR-0013 token gate, and busybox provides the shell — but two
   decisions come first. The server answers request/response only, so this needs either
   a hand-written WebSocket (handshake and framing, since nothing in the image provides
   them) or a long-lived response stream with a second route for keystrokes. And it is
   a root shell offered over plain HTTP on the LAN: the token stops a passer-by, not
   somebody reading the wire, so either the console gets TLS with a fingerprint shown
   on the attached screen, or the terminal is documented as trusted-network-only. Worth
   an ADR before any code, because the second decision changes ADR-0013's threat model.

**Hardware transcoding works.** `/api/gpu` on the reference laptop reports H.264 and
HEVC Main and Main10 decode *and* encode, plus VP9 decode, with GuC and HuC running —
the full set a Plex transcoding appliance needs. Getting HuC there took two fixes that
had to be found in the kernel source rather than guessed; both are in the trap list.
What remains unproven is Plex itself transcoding through it, which waits on Plex
running at all.

Still unproven: updates, rollback, and signing. Images are unsigned, so Secure Boot
must be off.

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
- **`mkusers` runs after `TARGET_DIR` is finished with.** Buildroot applies the users
  table while generating each filesystem image, into a copy — so `TARGET_DIR/etc/passwd`
  never gains the accounts, and anything in `post-image.sh` that reads `TARGET_DIR/etc`
  is reading the tree from before they existed. That is how the `plex` account ended up
  in `rootfs.erofs` and absent from the `/usr` image PlexOS actually boots. `post-image.sh`
  stage 0 now runs Buildroot's own `mkusers` against `TARGET_DIR` first, and stage 1
  refuses to build an image whose factory `/etc` is missing an account `users.table`
  declares. The Buildroot behaviour has not changed, so anything else added here that
  reads `TARGET_DIR/etc` has the same problem again.
- **The image had no TLS at all, and nothing said so.** No `ssl_client` applet, no
  `curl`, no `openssl`, no TLS library — while ADR-0010 fetches Plex from
  `downloads.plex.tv`, which serves HTTPS and nothing else. Provisioning could never
  have worked, and the symptom would have been a download failing on a machine whose
  network was demonstrably fine. `BR2_PACKAGE_OPENSSL`, `BR2_PACKAGE_LIBCURL_CURL` and
  `BR2_PACKAGE_CA_CERTIFICATES` fix it; the trust store is not optional, because
  `--insecure` would make the transport prove nothing.
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
- **`i915.enable_guc` defaults to auto, and auto means off below Gen12.**
  `uc_expand_default_options()` opens with `if (GRAPHICS_VER(i915) < 12) {
  enable_guc = 0; return; }`. Whiskey Lake-U is Gen9.5, so the driver never requests
  GuC or HuC firmware and shipping the blobs changes nothing. `i915.enable_guc=2` is
  `ENABLE_GUC_LOAD_HUC`; 3 would add GuC submission, which a transcoding appliance does
  not want. Without it HuC is silently absent and transcodes are worse at a given
  bitrate — the exact failure `plexos-gpu` exists to catch, which it could not do until
  debugfs was mounted.
- **Firmware for a built-in driver must be in the initramfs, not in `/usr`.** `i915` is
  `CONFIG_DRM_I915=y` and fetches firmware during `do_initcalls`, a second before
  `plexos-init` mounts `/usr`. Blobs in `/usr` are blobs the driver never sees, and it
  continues without them rather than retrying. `rootfs_initcall` unpacks the initramfs
  before `device_initcall` probes drivers, so that is the earliest place a file can be
  and still be found. `CONFIG_EXTRA_FIRMWARE` also works but needs an absolute path
  into the Buildroot target directory, which cannot live in a checked-in fragment.
- **The verity root hash cannot tell two images apart when only the UKI changed.** A
  kernel parameter or console setting leaves `/usr` byte-identical, so a reflashed
  machine and an untouched one report the same hash. `/api/status` reports the whole
  command line for this reason: it is the only field that distinguishes them, and
  without it "the fix did not work" and "the image was not flashed" look the same.
- **Nothing mounted cgroup v2, and the symptom would have been "Plex will not start".**
  `plan.rs` assembles the root from nothing, so the only filesystems that exist are the
  ones it mounts — and `/sys/fs/cgroup` was not among them. ADR-0007 bounds Plex with
  cgroup v2, so `cgroup::apply` could not create its directory, `plex::prepare` failed,
  and the child that becomes Plex was never spawned. On a machine whose kernel has every
  controller compiled in. Found by reading the plan against the trap two entries above,
  not by booting.
- **A controller enabled in the cgroup root is not available to a child.** cgroup v2
  requires the parent to name it in `cgroup.subtree_control` first. `cgroup::delegation`
  and `cgroup::missing_controllers` existed for exactly this and had no caller but their
  own tests, so `memory.max` would simply not have existed in Plex's cgroup: `apply`
  logs that it could not write the limit, and Plex runs unbounded. Two functions with no
  caller is the same shape as the `auth` defect — worth grepping for.
- **A program in the image is not a program that can do the job, and the build host
  proves nothing.** Twice in a row, in the same shape. `busybox tar` is present and was
  built without `FEATURE_SEAMLESS_XZ`, so it cannot read either member of a Debian
  package — while the build host has GNU tar. The target's `mkfs.erofs` was configured
  `--disable-lz4`, so it cannot compress an app image — while `post-image.sh` builds
  `/usr` with `lz4hc` all day through `host-erofs-utils`, which is a *separate build of
  the same package*. Both failed minutes into provisioning, after an 83 MB download and
  a signature verification, with a message that appeared to be about Plex's package.
  `Tools::find` resolves programs up front because reporting a missing one after the
  download is poor; `execute::check_compressor` now goes further and asks whether the
  program can do the thing, before anything is fetched. When adding a package for the
  target, check its sub-options — the default is usually the smallest build.
  Third instance, and a different flavour: busybox's `losetup` is not a smaller build of
  util-linux's, it is a *different program*, and it has no `--show` — so it cannot report
  which device it attached, which is the one thing mounting an app image needs. The fix
  there is to own the path deliberately: enable the full package's applet and turn
  busybox's off in the fragment, so the result does not depend on which package Buildroot
  installed last.
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
