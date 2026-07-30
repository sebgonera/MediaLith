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
| `crates/plexos-types` | Done. Formats and the layout emitter, 50 tests. The ADR-0006 manifest schema was reconciled with the artefacts PlexOS actually builds — one UKI per slot, and a `release` string `OsVersion` cannot express — which was the last moment that was an edit rather than a migration. |
| `crates/plexos-update` | Which slot an update goes to, writing a partition and reading it back, the ADR-0006 trust chain, the anti-rollback sequence, root-signed revocation, boot-entry/slot agreement, and `plexos-sign` as the publisher's half. 65 tests. **Has updated the reference laptop four times, alternating slots — and one of those updates was deliberately unbootable and was rolled back.** All four were unsigned, through an improvised `update.json` this crate no longer parses. **Nothing signed has yet reached a machine.** |
| `crates/plexos-gpu` | 46 tests, and it has now answered the question it was written for — on four machines, three of which it was wrong about until they were tried. On the reference laptop: UHD 620, iHD 26.1.2, VA-API 1.23, GuC and HuC both running, verdict `ready`. |
| `crates/plexos-sys` | The kernel-interface layer, and the only crate allowed `unsafe`: verity superblock, dm ioctls, mount, exec/execve, partition labels, Landlock, privilege dropping, `reboot(2)`, `sethostname(2)`, PTY allocation for the console terminal, and reaping children for PID 1. 86 tests. The boot syscalls have run on real hardware; Landlock is proven by `examples/landlock-demo` on a build host and now by Plex running under it on the appliance; privilege dropping has run, dropping to 900:900 before `execve`. |
| `crates/plexos-init` | Plans and executes the boot, and runs as PID 1 in both roles. The supervisor role mounts the Plex app image, then keeps the console and a shell running: it reaps orphans, restarts what dies with a widening delay, and never exits. 62 tests, none of them on hardware yet. |
| `crates/plexosd` | Network diagnostics on the page (ADR-0012), the health gate (now run after Plex starts, with a real loopback probe), boot-counter clearing, and the status console (ADR-0012): wired-network bring-up, a hand-written HTTP server, the page, the ADR-0013 device token and the gate that enforces it, mounting the Plex app image at boot, claiming the device at first start, provisioning Plex in the background, starting it confined, and stopping the machine cleanly from the page. Also ADR-0005's enforcement: restarting on an unhealthy boot when the entry is still being counted, recording on `/var` why a slot was given back, and clearing away the boot entries of failed updates, the configuration model actually applied (ADR-0008), and the terminal session (ADR-0014), and the updater on the signed manifest. 242 tests. **Working on the reference laptop:** the appliance brings up its own network, takes a DHCP lease, and serves the page to a browser on another machine. It took three boots and three faults to get there — bring-up ordering, `PATH`, and a missing `/tmp` — each hidden behind the one before it. |
| `crates/plexos-plex` | Provisioning Plex from its own signed packages (ADR-0010, ADR-0007): reads the `.deb`, verifies `_gpgplex` against a pinned key, ties it to the payload, builds an erofs app image, manages the version store, mounts it with the hash checked first, bounds it with cgroup v2, and holds the confine-then-exec sequence. 104 tests. Provisioning now runs end to end **on the appliance**, driven from a browser: download, signature, manifest, build, publish, mount, confine, start. |
| `buildroot/` | Builds. defconfig, kernel fragment, a users table for the `plex` account, and packages for `plexos-init`, `plexosd`, `plexos-gpu`, `plexos-systemd-boot` and `plexos-plex-keyring`. |
| `post-image.sh` | All stages run, and produce an image that boots on hardware. Stage 0 applies the users table, which Buildroot itself applies too late to reach `/usr`. 47 checks in `post-image-test.sh`, none skipped on a machine with the Buildroot tree. |
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

**Updates work over the network, and the try counter has been exercised.** The
appliance was updated twice from a browser's request with no USB stick: slot A to B and
back, each time fetching a bundle from the build host, writing the inactive slot, reading
it back, installing a boot entry on trial and restarting into it. The last boot produced
the thing this project had never seen — `plexos-0.1.0.202607281844+2-1.efi` renamed to
`plexos-0.1.0.202607281844.efi`, which is systemd-boot decrementing the try counter and
the health gate then declaring the slot permanent — the *success* half of ADR-0005.
Nothing signs a bundle yet; what makes that survivable is that a bad one costs three
reboots and lands back on the system that worked.

**A person can now install Plex from a browser, and it works.** On the reference
laptop: the appliance claims itself and prints a sixteen-character token on the attached
screen, the console page takes that token, `POST /api/provision` downloads Plex from
Plex's own endpoint, verifies its signature against the pinned key, builds an erofs app
image, mounts it and starts Plex confined — cgroup, Landlock, uid 900 — and Plex then
serves its own interface on port 32400 and was claimed to a Plex account.

Five image faults stood between "written" and "works", and every one of them was a
program that was present and could not do the job, or a policy that denied something
nobody had listed: `tar` without `.xz`, `mkfs.erofs` without a compressor, `losetup`
without `--show`, a Landlock policy missing `/usr` so nothing could execute, and the same
policy missing `/run` so `/etc/resolv.conf` — a symlink — could not be followed and DNS
silently failed. All five are in the trap list. The lesson they share is in there too:
capability is not presence, and a deny-by-default policy has to be executed before it can
be believed.

**And it transcodes on the GPU.** 4K HDR10, HEVC Main 10, decoded and re-encoded to
1080p HEVC with `(hw)` on both ends — which is the sentence this whole project was
written to be able to write, and exactly the capability set `plexos-gpu` predicted from
the hardware alone months before Plex existed on the machine.

Next, in order:

1. **A supervisor.** Half done and **unproven on hardware**. `plexos-init` no longer execs
   a shell: it stays alive as PID 1, reaps orphans, restarts a service that dies with a
   widening delay, and respawns the console shell like a getty. What is left is the other
   half — `plexosd` supervising Plex, so that a Plex which exits comes back and a
   newly-provisioned version replaces a running one without a reboot. `plexosd::plex` says
   so rather than pretending. This is the largest piece of missing *function*, as opposed
   to missing trust.

   **Changing PID 1 is the riskiest edit in this repository**, and the next image is the
   one that finds out: a supervisor that exits panics the kernel, and there is no console
   to read the reason on if the thing that failed is the one that starts the console. There
   is a way back — the other slot still holds `0.1.0.202607291945` — but it costs the
   three boots ADR-0005 charges.

2. **Prove the other rollback path.** The unbootable-image branch has run (below). What
   has not is the one where the image boots and the system does not work — the gate
   restarting to spend a try, and the record it leaves on `/var`. That code is written and
   tested and has never executed on hardware. Staging it needs a bundle that boots but
   whose Plex cannot start, which is a realistic bad update and not obviously easy to
   build deliberately.

3. **Installer and first-boot wizard.** Never started, and the reason it has not mattered
   is that the only installs so far were `dd` onto a disk by somebody who wrote the image.
   A machine handed to anybody else needs both.

4. **`xe` firmware is not in the image at all**, only `i915/`. Found while fixing the
   GuC/HuC list. `CONFIG_DRM_XE=y`, so Arc parts bind — but a driver without its firmware
   is the thing that just cost an evening, and the claim that current Arc works today is
   softer than it looked.

5. **Upload from a local disk**, and the removable-media path of ADR-0010. Both were
   asked for and both are deferred: an 83 MB upload has to stream to disk, and
   `http::MAX_BODY` is deliberately 64 KiB so that route reads the socket itself.

6. **NVIDIA (ADR-0015).** Planned in detail, deliberately unscheduled. The blocker is
   `CONFIG_MODULES=n`, not the driver. Steps 1 and 2 of that ADR are about half a day and
   answer most of the risk.

7. **TLS on the console (ADR-0014).** Sequenced after update signing and now due: with the
   update path closed, the console's root shell over plain HTTP is the widest opening left.

**Hardware transcoding works.** `/api/gpu` on the reference laptop reports H.264 and
HEVC Main and Main10 decode *and* encode, plus VP9 decode, with GuC and HuC running —
the full set a Plex transcoding appliance needs. Getting HuC there took two fixes that
had to be found in the kernel source rather than guessed; both are in the trap list.
Plex now transcodes through it: 4K HDR10 HEVC Main 10 to 1080p HEVC, `(hw)` on the
decode and `(hw)` on the encode. The verdict `plexos-gpu` reached from sysfs and vainfo
turned out to describe what the machine actually does.

**And a bad update has now undone itself, with nobody touching the machine.** A bundle
whose `/usr` had its first block overwritten — hash tree and root hash left intact, so
every check in the update path passed — was installed to the inactive slot and booted.
The appliance went unreachable at 13:27:09 and answered again at 13:33:33 running the
previous version from the previous slot, with the bootloader's own bookkeeping left on
the ESP: `plexos-0.1.0.202607291323+0-3.efi`, three tries offered and three used.

Getting there needed two fixes first, both of which meant ADR-0005 did not work at all,
and neither of which any test could have caught. `panic_timeout` defaults to 0, so a
machine that could not verify `/usr` sat at a panic screen forever with three unused
tries — the counter is spent by *booting*, so a failed boot has to end in another one.
And an unhealthy boot that reached userspace left the counter standing and then nothing
restarted, so nothing consumed it. The experiment then found two more, in the wreckage it
left behind: an exhausted entry still carries a counter in its name, so it read as "on
trial" forever, and nothing ever deleted it from an ESP sized for three UKIs.

**The console has a terminal, and settings that change the machine.** ADR-0014 records
the two decisions that had to come first: long-polling rather than a hand-written
WebSocket behind a root shell, and a documented network boundary — this console is for a
trusted LAN and is not fit to expose beyond one. TLS is sequenced after update signing,
because closing the console while an unsigned update path lets anyone on the wire choose
what `/usr` runs would protect the smaller opening.

**The console answers the three network questions now.** `/api/network` reports the
resolver with its symlink target, the default route, and whether `downloads.plex.tv`
actually resolves — in 88 ms on the reference laptop. It found a defect in itself on its
first real run: udhcpc writes the interface as a trailing comment, so the nameservers
came back as `8.8.8.8 # eth0`, while a test whose fixture was imagined rather than
captured passed throughout.

**Updates are signed, and the appliance has proved it on hardware.** `0.1.0.202607301205`
was installed over the network by the *old* unsigned updater — the last bundle that will
ever be accepted that way — booted on slot A, and the health gate then made the slot
permanent (`+2-1` renamed to no counter). The image it brought up refuses everything it
should, checked against a real machine rather than against a test:

- an **unsigned** bundle: refused, naming `tools/sign-bundle.sh` as the remedy;
- a **tampered** manifest, one field changed after signing: refused, and the message names
  reformatting as the likely innocent cause, which is the mistake a person actually makes;
- a **replayed** older release, correctly signed by the real key: **the signature verifies**
  — the console shows the key and the root that certified it — **and the update is refused
  anyway**, because sequence 202607010000 is below the 202607301205 already accepted. That
  is the one case the counter exists for, and it is the one no signature check can catch.

The anti-rollback floor came from the running image's own build stamp, with nothing
recorded on `/var` yet — the path that protects a machine installed by `dd`, which is all
of them.

The root key is a development key: its private half is on the build host, and every place
that reports a signature says so, including the appliance's own log line.

Still unproven: revocation, which has tests and no history, and the half of rollback where
the image boots but the system does not work. **Kernel images are still unsigned, so Secure
Boot must be off** — that is ADR-0004 and separate from update signing.

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
- **Nothing brought loopback up, and the error named neither loopback nor a network.**
  `net::candidates` excludes `lo` deliberately — it is not something to run DHCP on, and
  `127.0.0.1` is never the answer to "what address do I type into a browser". Nothing
  else touched it, so `lo` stayed down. Plex binds a listener on `127.0.0.1`, got
  `EADDRNOTAVAIL`, and died with an uncaught C++ exception from inside Boost.ASIO — a
  message mentioning `boost/asio/detail/reactive_socket_service.hpp` and nothing else.
  The health gate's `plex-http` probe goes over loopback too and reported it as Plex not
  answering. Bringing the interface up is the whole fix: the kernel adds `127.0.0.1/8`
  itself on `NETDEV_UP` for a device with `IFF_LOOPBACK` (`net/ipv4/devinet.c`), so there
  is no address to assign and no second step.
- **A confined child's output has to be captured, or its failure is invisible.** Plex's
  child inherited stdout and stderr, so the confinement log and Plex's own dying words
  reached only the attached console. Two failures in a row had to be diagnosed by
  re-running the policy on a build host and reasoning backwards. `plexosd` now pipes both
  streams, drains them on threads and serves the tail from `/api/provision`; the third
  failure was read off the network in one request and identified in a minute.
- **Landlock follows symlinks out of a granted directory, and musl does not complain.**
  `/etc/resolv.conf` is a symlink to `../run/resolv.conf` — Buildroot's skeleton makes it
  one so a read-only `/etc` can still have a lease-managed resolver. Granting `/etc`
  therefore does not grant the file: Landlock resolves the symlink and checks the target,
  which was in `/run`, which was not granted. musl reports none of this — it falls back
  to `127.0.0.1`, where nothing listens, so every lookup fails with "Could not resolve
  host" on a machine whose DNS is fine from a shell. That is what stopped the Plex server
  being claimed. Grant the *directory* rather than the file: `udhcpc` rewrites
  `resolv.conf` on every renewal, and a rule tied to the old inode would stop covering
  the new one, giving DNS that works until the first lease renewal.
- **A placeholder that was correct once becomes a lie later.** The boot gate's
  `plex-http` check was wired to `&|| false` — a literal "no probe" — with a comment
  saying Plex was not in the image yet. That was true and harmless for months. The moment
  Plex was installed the check became applicable and reported "installed but not
  answering" about a server that was answering fine, on every boot, so the try counter was
  never cleared. Two separate defects wore one symptom: the missing probe, and the gate
  running before anything started Plex. Grep for stub closures and `unimplemented` paths
  whose comment begins "not yet".
- **`post-image.sh`'s stages run in one order and it is easy to write into a tree that
  has already been sealed.** `os-release` was being written in stage 4, where the UKI is
  assembled, and the `/usr` image is built in stage 1 — so the boot entry carried the
  right version and `/usr/lib/os-release` still said `Buildroot 2026.02.3`. Harmless
  until `plexos-update` began comparing that string against a bundle's: `2026` sorts
  above `0`, so every update would have been refused as older, with a message blaming
  whoever published it. Anything that must appear *in the image* has to be written before
  stage 1, and the check is to extract the built image and look — not to read the script.
- **`make all` does not rebuild a package whose sources changed.** Buildroot rsyncs a
  package's tree into `output/build/<pkg>/` once and does not re-sync one it has already
  built, so a plain `make all` ships the *previous* binary under a new version stamp. Two
  update bundles went out that way and the appliance updated successfully into a system
  functionally identical to the one it was running: version and slot changed, the fixes
  did not. `make <pkg>-rebuild` forces the re-sync. Check by grepping
  `output/build/<pkg>/` for something the change added, not by reading the script.
- **A control that is correct in the state it was written in can be wrong in the state it
  leads to.** Twice, in one shape. The `plex-http` probe was `|| false` and correct while
  Plex could not exist. The device-token field lived inside the Plex install card and was
  correct until Plex was installed — after which that card renders as a single link, the
  field is gone, and every button needing a token silently refuses. Ask what a piece of
  interface looks like in the state its own success produces.
- **A `CONFIG_*` symbol at `=y` does not mean the feature you want is present.**
  `CONFIG_NFS_V4=y` gives NFS 4.0 and nothing later; 4.1 and 4.2 are separate symbols and
  were off. A mount asking for `vers=4.2` therefore came back `EINVAL` — an error about
  *arguments*, which reads as a malformed option string and says nothing about versions.
  Four build-and-reboot cycles were spent varying options that were never the problem.
  This is the same trap as `BR2_PACKAGE_EROFS_UTILS` without its `_LZ4` sub-option,
  already recorded here, walked into from the kernel side: **check the sub-symbols, and
  check them against the feature you are about to ask for by name.**
- **When a diagnosis costs a build cycle, stop guessing after the first one.** Each of
  those four attempts cost a build, a bundle, an update and a reboot, and none of them was
  informed by evidence — the kernel had already written the reason to its ring buffer and
  nothing here could read it. Reach for the log, or ask the person with a shell, before
  the second guess.
- **A fixture you imagined is a test that agrees with your code and not with the
  machine.** `resolv.conf` was parsed with the comment rules guessed rather than captured
  — udhcpc writes `nameserver 8.8.8.8 # eth0`, with the comment at the end of the line,
  and the test put comments on their own line. The parser reported `8.8.8.8 # eth0` as an
  address on the appliance while its test passed. Same rule as `CONFIG_*` symbols and PCI
  IDs, applied to the output format of any program whose file you read.
- **A design can be complete, tested and uncalled, and the tests will not tell you.**
  Three times now: the `auth` gate, `cgroup::delegation`, and the whole ADR-0008
  configuration model — schema, validation, fixtures, and `paths::CONFIG_FILE` with no
  callers anywhere, so no hostname was ever set and no timezone ever applied. Grep for
  callers of a constant before assuming the feature behind it exists.
- **Storing is not applying, and a settings page that conflates them is worse than none.**
  It looks like it worked. `plexosd::settings` reports four distinct outcomes per field
  for that reason, and the sharpest case is the timezone: with no zoneinfo in the image,
  pointing `/etc/localtime` at a missing file *succeeds* and every program then falls back
  to UTC in silence.
- **Plex downloads its own encoders, and a sandbox that cannot run them fails somewhere
  else entirely.** EAC3, TrueHD and DTS do not go through ffmpeg here: Plex fetches
  EasyAudioEncoder at runtime into `Codecs/` under `/var/lib/plex` and runs it as a
  separate process. Granted read and write but not execute, the download succeeds, the
  file is 0755, it runs perfectly from a shell — and never starts under Landlock. The
  screen says "EasyAudioEncoder failed", the log says "EAE not running, or wrong folder?"
  and names a folder that is correct, and the film that played yesterday was one whose
  audio happened not to need it. Third instance of the same shape after `/usr` and
  `/run`: a deny-by-default policy missing something nobody listed, found only when
  something finally asked for it.
- **No render node and no graphics card look identical through `/sys/class/drm`.** A
  `renderD*` node appears only after a kernel driver binds, so a machine whose card the
  kernel cannot drive enumerates as zero GPUs — exactly like a machine with none. The
  report said "No graphics device found" and advised enabling the integrated GPU in
  firmware, to somebody running a discrete RTX 5060 in a system that has no integrated
  graphics. `plexos_gpu::display_devices` reads the PCI bus so the three states are told
  apart: nothing there, something there with no driver, and a driver bound that produced
  no render node.
- **There is no `udev`, so a DRM render node is `0600 root:root` and Plex cannot open
  it.** DRM sets no mode on its device nodes, `devtmpfs` therefore creates them
  root-only, and every ordinary distribution relaxes the render nodes with a rule like
  `SUBSYSTEM=="drm", KERNEL=="renderD*", MODE="0666"`. Nothing here did. The reason it
  took a second machine to find is that **every layer above reports success**:
  `plexos-gpu` says `ready` with the full capability list because it probes as root,
  `vainfo` works from a shell for the same reason, and the Landlock grant on `/dev/dri`
  is correct and grants nothing — Landlock only ever restricts what the ordinary
  permissions already allow. Only Plex fails, and it fails by quietly using the CPU.
  Fourth thing to assume udev existed, after `/dev/mapper`, the two `by-partlabel`
  lookups, and this.
- **A report that probes as root is answering about the wrong process.** The GPU report
  now checks whether the render node's `other` bits are set, because "the hardware can do
  this" and "the account Plex runs as can reach it" had never been the same question.
- **A firmware list written for one machine is a firmware list that works on one
  machine.** `install_gpu_firmware` shipped two blobs — the Kaby Lake pair Whiskey Lake-U
  asks for — and was correct for a month. On an Alder Lake laptop i915 asked for
  `adlp_guc_70.bin`, found nothing in the initramfs, and *carried on*: hardware
  transcoding worked and produced worse quality than the chip can manage. The blob was in
  `/usr` the whole time. It globs every GuC and HuC blob now, about 25 MiB, which lands
  in both UKIs and twice in every bundle — the price of an image that works on the
  hardware it is put on rather than the hardware it was built on.
- **`Unknown` is not licence to guess a cause.** The GPU report saw a debugfs value it
  did not recognise and reported "debugfs is not mounted" — about a file it had just read
  successfully. That guess hid the missing firmware above for as long as nobody changed
  machines. The parser knows `status: MISSING` and `status: ERROR` now, and the remaining
  unknown case says it does not know which of two things is true.
- **A wrong remedy is worse than none.** `could not bind :80` first suggested "pass a
  higher port", which is right for `EACCES` and actively misleading for `EADDRINUSE`,
  where the port is fine and something else holds it. Match the remedy to the error
  kind, not to the operation that failed.
- **`panic_timeout` defaults to 0, and 0 means loop forever.** Every automatic-recovery
  scheme that ends in a kernel panic needs `panic=N` on the command line, or the machine
  simply stops. ADR-0005's counter is spent by *booting*, so a boot that fails has to end
  in another boot — and for the whole life of the project a failed one ended in a panic
  screen with three unused tries, which turned "undoes itself with nobody present" into
  "hold the power button three times". Neither the absence nor a wrong value is visible
  from outside, which is why `post-image-test.sh` now asserts it.
- **An exhausted boot entry is still "on trial" by its name.** `plexos-<v>+0-3.efi`
  carries a counter, so `tries_left.is_some()` is true and the wreckage of a failed update
  satisfies every naive on-trial test. It made the gate announce an impending rollback on
  a machine where nothing could roll back, and it would have made the *next* update see
  two entries on trial, fail to tell which had booted, and silently stop rolling back at
  all — the mechanism working exactly once per machine and then disabling itself. Ask
  `is_exhausted` as well, everywhere `is_on_trial` is asked.
- **Nothing removed the wreckage, on the one partition the machine cannot boot without.**
  Each failed update leaves an 18 MB UKI on an ESP that ADR-0003 sized for three.
  `install_entry` deliberately never removes the entry that works, which is right, and
  that principle quietly covered a case it should not have. An exhausted entry is the
  safest thing on an ESP to delete — except when it is the one that booted, which is what
  two bad updates in a row produce.
- **A rollback destroys its own explanation.** Reverting `/usr` takes the log, the gate's
  verdict and the version string with it, and the system that comes back is the older one,
  which cannot tell it is a replacement. `/var` is the only surface that survives, and it
  survives because of the rule that makes it awkward everywhere else.
- **Two outcomes that both do nothing still need different words.** A check that found a
  newer release and a check that found none took the same `Ok(None)` out of the updater, so
  the page said "already up to date" directly underneath a line naming the version it had
  just found. Both are true statements about the machine and only one answers the question
  that was asked. Found in the first minute of driving the signed path on the appliance,
  having survived every test, because a test asserts what a function returns and this was a
  defect in what the return *meant*.
- **A schema written before the artefact exists describes an artefact that does not
  exist.** `plexos-types::manifest` had one `uki` field and one `os_version` of the form
  `MAJOR.MINOR.PATCH`. PlexOS builds two UKIs, because `plexos.slot=` is on the command
  line *inside* one, and stamps its version `0.1.0.202607281844`, which that type cannot
  hold. Both were written months before either artefact existed, both had passing
  fixture-based tests, and neither could have carried a real update. What made it cheap was
  luck rather than judgement: no appliance had ever parsed a manifest, so a crate that is
  append-only because its formats reach disks had one format that never had. Check a schema
  against a built artefact before the first machine reads it, because after that the same
  edit is a migration.
- **A signed document has to be fetched as bytes.** `String::from_utf8_lossy` replaces
  anything invalid with U+FFFD and does it silently, so a manifest fetched as text parses
  fine, verifies against nothing, and reports a signature failure about a document nobody
  mistyped. `fetch_bytes` exists separately from `fetch_text` for that one reason. The same
  trap eats a re-serialisation: the signature covers the bytes that arrived, and even
  reindenting the file breaks it -- confirmed by doing it.
- **Record the anti-rollback floor after the install, never before.** Raising it is
  permanent and there is deliberately no way to lower it from the network, so recording a
  sequence before the boot entry is installed means one failed download refuses that
  release forever: an appliance that will not take the update it just failed to finish.
- **A deliberately broken bundle has to be signed like a real one.** `tools/break-bundle.sh`
  re-signs after corrupting the image, which feels wrong and is exactly right. An
  experiment that skipped it would be testing the signature check -- which has its own
  tests -- and would prove nothing about ADR-0005, while looking like a rollback that
  worked.
- **Break the image, not the manifest, when testing rollback.** Overwriting a data block
  and recomputing only the manifest digest leaves the hash tree and root hash intact, so
  the updater accepts the bundle — correctly, because every check it makes asks whether the
  bytes offered are the bytes stored. Only dm-verity can know, and only at boot. Breaking
  the manifest instead tests the updater's parser and proves nothing about ADR-0005.
  `tools/break-bundle.sh` does the former, and `veritysetup verify` will confirm the
  premise on the build host before anything is sent to a machine.

## It has now run on four machines in two days

The reference laptop (UHD 620), an RTX 5060 desktop with no integrated graphics, an Alder
Lake-P laptop, and back to the reference laptop — the USB stick simply moved. Every one of
those moves found a defect that had been latent for weeks, and none of them was found by
reading code:

- **The RTX machine** had no driver bound at all, and the GPU report advised enabling an
  integrated GPU it does not have. Now it reads the PCI bus and names the device.
- **The Alder Lake laptop** had a fully working VA-API stack, `ready` health, and Plex on
  the CPU: the render node was `0600 root:root` because there is no `udev` here, and every
  probe above it runs as root.
- **The same laptop** then transcoded on the GPU at reduced quality, because the initramfs
  carried GuC/HuC firmware for exactly one generation — the one the reference laptop needs.

The lesson they share is the one this file keeps recording in different words: a thing that
is true about the machine it was written on is not a thing that is true.

## Other hardware it has been tried on

A machine with **no integrated graphics and an RTX 5060** (`10de:2d05`, Blackwell). It
boots, serves the console, and transcodes on the CPU. No kernel driver binds to the card:
this kernel builds `i915` and nothing else, so `/sys/class/drm` holds only `version` and
`/dev/dri` never exists.

Supporting it is not a kernel option away, and ADR-0015 works out why. The blocker is not
NVIDIA: **`# CONFIG_MODULES is not set`.** This kernel cannot load a module at all, and an
out-of-tree driver cannot be built in — so the first step is admitting loadable modules to
an image whose defining property is that it is one artefact built from source and covered
by one root hash. After that comes a package building NVIDIA's open modules (Blackwell
requires them; they are dual MIT/GPL, which is the good news), GSP firmware, and a
binary-only userspace that Plex reaches NVDEC and NVENC through. Buildroot's
`nvidia-driver` is pinned at **390.151**, a Kepler-era branch, and is no help.

`CONFIG_DRM_XE=y` is already set, so current Intel Arc cards work today with no change.
`CONFIG_DRM_AMDGPU` is not set; it is the cheapest coverage available but is deliberately
unscheduled, because none of the hardware this actually runs on is AMD.

ADR-0015 breaks the NVIDIA work into eight steps and names the two that would stop it: the
open modules failing to build against 6.19 — their "4.15 or newer, no maximum" is a claim,
not a test — and `/dev/nvidia*` needing a setuid helper this image will not carry, since
there is no `udev` here. The size question is settled: `usr_a` is 1 GiB and the image uses
73.6 MiB, so nothing about this touches the frozen partition layout.

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
