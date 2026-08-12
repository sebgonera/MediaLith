# MediaLith — working notes

An immutable, atomically-updated Linux appliance distribution built to run Plex Media
Server well. Read `docs/ARCHITECTURE.md` first, then `docs/adr/` for why anything is
the way it is, and `docs/PRODUCTION-READINESS.md` for what still stands between this and
a unit somebody else could be given.

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

## Rules for the work itself

Not about MediaLith. About mistakes made *while building* it, every one of which happened
more than once, and none of which the "Known traps" list could have prevented because that
list is about the system. Check against this before committing; it is short on purpose.

1. **After a scripted edit, prove the edit landed — and prove it landed nowhere else.**
   Every `str.replace` that silently matched nothing has cost something: the first-boot
   section shipped with its markup never inserted, because the anchor
   `<main class="grid" id="cards">` did not exist and nothing said so. `grep` for the new
   text, or diff, before moving on. A replacement that matched nothing is not an error in any
   tool that will tell you.
   The other half cost an evening too, and is worse because it *looks* like it worked. A
   word-boundary rename over the activity card's helpers — `tile` → `metricTile` and six
   others — also rewrote `ring.push(value)` into `ring.pushSample(value)`, five CSS class
   names inside template literals, and five doc comments. Every one was a silent break: the
   page still parsed, still addressed every id, and would have rendered as unstyled text.
   **A rename is over identifiers, so a rename that also hits strings, class attributes and
   prose was applied with the wrong tool.** Diff the whole hunk and read it, or bound the
   replacement to declarations and call sites.
2. **A change to `ui/console.html` is not done until the *served* page has been checked.**
   Twice in one day a page change reached the appliance broken — a duplicate `const`, which
   is a parse error that blanks every section, and a section whose markup was missing, whose
   exception its own poll swallowed. Both passed tests that assert strings appear in the
   page, because in both cases the strings were in the script. After deploying: fetch `/`,
   extract the script, run `node --check`, and check every `getElementById` against what the
   page creates. Two tests do this at build time now; run them against the *fetched* page as
   well, because that is the artefact.
3. **Never `rm -rf` a directory you did not wholly create.** `crates/plexosd/examples/` was
   deleted twice to remove one scratch file, taking `authgate.rs` with it both times. Remove
   the file by name. Better: put scratch probes outside the tree entirely.
4. **A test's scratch path includes the test's name.** Rust runs tests as threads in one
   process, so a fixed path is a race — one test deleting what another is reading. Written
   wrong twice; suspected in a third that still flakes about once in twenty runs.
5. **Do not conclude a package was not rebuilt by grepping its binary for a string.** A
   string that nothing prints is one the compiler discards, so its absence proves nothing.
   That produced a false alarm about `plexosd`. Grep for a string that is actually used, or
   look at the sources in `output/build/<pkg>/`.
6. **Ask what the *state its own success produces* looks like.** Already in the traps list
   for the system; it applies to the work too. Every one of the mistakes above was invisible
   in the state it was written in and only wrong in the state that followed.

## Where things stand

| Component | State |
| --- | --- |
| `crates/plexos-types` | Done. Formats and the layout emitter and the GPT writer, 65 tests. The ADR-0006 manifest schema was reconciled with the artefacts MediaLith actually builds — one UKI per slot, and a `release` string `OsVersion` cannot express — which was the last moment that was an edit rather than a migration. |
| `crates/plexos-update` | Which slot an update goes to, writing a partition and reading it back, the ADR-0006 trust chain, the anti-rollback sequence, root-signed revocation, boot-entry/slot agreement, and `plexos-sign` as the publisher's half. 65 tests. **Has updated the reference laptop four times, alternating slots — and one of those updates was deliberately unbootable and was rolled back.** All four were unsigned, through an improvised `update.json` this crate no longer parses. **Nothing signed has yet reached a machine.** |
| `crates/plexos-gpu` | 46 tests, and it has now answered the question it was written for — on four machines, three of which it was wrong about until they were tried. On the reference laptop: UHD 620, iHD 26.1.2, VA-API 1.23, GuC and HuC both running, verdict `ready`. |
| `crates/plexos-sys` | The kernel-interface layer, and the only crate allowed `unsafe`: verity superblock, dm ioctls, mount, exec/execve, partition labels, Landlock, privilege dropping, `reboot(2)`, `sethostname(2)`, PTY allocation for the console terminal, reaping children for PID 1, resolving partitions on a *named disk* rather than by label alone, and `statvfs(3)` — free space is the one thing this appliance reports about itself that is not readable as a file. 100 tests. The boot syscalls have run on real hardware; Landlock is proven by `examples/landlock-demo` on a build host and now by Plex running under it on the appliance; privilege dropping has run, dropping to 900:900 before `execve`. |
| `crates/plexos-init` | Plans and executes the boot, and runs as PID 1 in both roles. It also lets the attached screen go dark after five idle minutes — two escape sequences from `console_codes(4)` written once to `/dev/tty0`, which needs no package and no daemon, because `setterm` is `util-linux` and not in this image. The supervisor role mounts the Plex app image, then keeps the console and a shell running: it reaps orphans, restarts what dies with a widening delay, and never exits. It also asks the boot loader which disk the firmware started, so a two-disk machine mounts the right `/usr` and `/var`. 63 tests. **PID 1 stays alive on the appliance and has restarted both of its services after they were killed.** |
| `crates/plexosd` | Network diagnostics on the page (ADR-0012), the health gate (now run after Plex starts, with a real loopback probe), boot-counter clearing, and the status console (ADR-0012): wired-network bring-up, a hand-written HTTP server, the page, the ADR-0013 device token and the gate that enforces it, mounting the Plex app image at boot, claiming the device at first start, provisioning Plex in the background, starting it confined, and stopping the machine cleanly from the page. Also ADR-0005's enforcement: restarting on an unhealthy boot when the entry is still being counted, recording on `/var` why a slot was given back, and clearing away the boot entries of failed updates, the configuration model actually applied (ADR-0008), and the terminal session (ADR-0014), the updater on the signed manifest, a supervisor that restarts Plex and swaps a newly-installed version in without a reboot, the console's own TLS identity (ADR-0014), the installer and the first-boot flow (ADR-0016), live Plex activity behind the device token (ADR-0018 — the Plex account token is read from `Preferences.xml` per request, sent to loopback in a header, and has no field in the browser's document it could land in), and the activity card — what the machine is doing *now*, which is the only view here about a moment rather than a state. 407 tests, of which two fail on any development host running Plex; see the trap list. **The activity card has never run on the appliance**: its numbers were produced by replaying the appliance's own captured `/proc` and `/sys` through the real code, which is one step short of the machine. **Working on the reference laptop:** the appliance brings up its own network, takes a DHCP lease, and serves the page to a browser on another machine. It took three boots and three faults to get there — bring-up ordering, `PATH`, and a missing `/tmp` — each hidden behind the one before it. |
| `crates/plexos-plex` | Provisioning Plex from its own signed packages (ADR-0010, ADR-0007): reads the `.deb`, verifies `_gpgplex` against a pinned key, ties it to the payload, builds an erofs app image, manages the version store, mounts it with the hash checked first, bounds it with cgroup v2, and holds the confine-then-exec sequence. 104 tests. Provisioning now runs end to end **on the appliance**, driven from a browser: download, signature, manifest, build, publish, mount, confine, start. |
| `buildroot/` | Builds. defconfig, kernel fragment, a users table for the `plex` account, and packages for `plexos-init`, `plexosd`, `plexos-gpu`, `plexos-systemd-boot` and `plexos-plex-keyring`. |
| `post-image.sh` | All stages run, and produce an image that boots on hardware. Stage 0 applies the users table, which Buildroot itself applies too late to reach `/usr`. 47 checks in `post-image-test.sh`, none skipped on a machine with the Buildroot tree. |
| Installer, updater, first-boot wizard | Not started. |

**MediaLith is installed on the reference laptop's internal disk and boots from it.** Its own
installer put it there (ADR-0016); the USB stick it was installed from is still attached and
still holds a working system, which makes it the recovery medium. Everything below that
says "USB stick" is history rather than the present arrangement.

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

Next, in order. The three items that stood here — `xe` firmware, upload from a local disk,
and the NVIDIA spike — are all done, and all three sat here after they were done, which is
the drift this file warns about happening to this file. The list now names one thing and
then points at the document that tracks the rest, because a list kept in two places is a
list that goes stale in one of them.

1. **Enrol a Secure Boot key in firmware, and boot with it enforcing.** The signing half is
   proven: ADR-0017 chose own keys, `post-image.sh` signs the bootloader *and* both UKIs,
   and a signed image booted. No firmware has ever enrolled the key, so nothing has yet
   booted with Secure Boot on — and until something does, the kernel command line is
   editable by anyone holding the machine, which quietly weakens every claim in the tree
   that leans on the command line living inside a signed UKI. `PK` is what switches
   enforcement on: with only `db` enrolled the platform stays in Setup Mode while the setup
   screen reports otherwise. Both slots must carry signed UKIs before it is switched on, or
   ADR-0005's rollback lands on a UKI the firmware refuses.

2. **A MediaLith update service that exists.** ADR-0020 built the appliance's half and the
   publishing half, and proved both on hardware — but there is no host, no domain and no
   published tree, so every appliance ships with `[update_service].url` empty and never
   looks. The remaining work is not code: somewhere to put a static directory, and the
   clock-synchronisation item below, because a machine whose RTC is wrong cannot validate
   TLS against a real service and this deliberately does not weaken that.

3. **`docs/PRODUCTION-READINESS.md`** — the distance between this and something that can be
   handed to a person who did not build it, with a revision note recording what has closed
   since it was written. Nothing in it is architectural. The items with no owner: a
   development root key with no ceremony and no way to revoke *itself*, no update channel
   or discovery, no clock synchronisation of any kind, nothing ever writing `/var/log`,
   nothing pruning `/var`'s largest writer, and no ceiling on connections.
   Free space is half closed: `plexos_sys::fs::space` reads it and the activity card shows it
   against a severity threshold, so a filling `/var` is now visible. **Nothing refuses on
   it** — the update and provisioning paths still stage an ~85 MB download without asking
   whether it fits, which is the half that turns a full partition into a failure rather than
   a warning.

4. **Run the hardware that has never been run.** `xe` has its firmware in the initrd, the
   `xe` debugfs layout is now read the way `xe` actually creates it, and no Arc card has
   ever been plugged in — so the claim is shipped-and-unverified rather than false, and the
   only thing that can close it is a card.

## Done, and proven on the machine (ADR-0016)

**The installer and the first-boot flow.** The only installs so far
were `dd` onto a disk by somebody who wrote the image; a machine handed to anybody else
needs both. The installer is a mode of the same image and writes **the system that is
running** to a chosen disk — no second artefact to build, sign or keep in step, and what
lands is what dm-verity already verified and this hardware already booted.

**Done:** `plexos_types::gpt`, which turns ADR-0003's frozen layout into the bytes of a
partition table. No new package in the image, because "a program in the image is not a
program that can do the job" has cost this project three evenings and the most
destructive operation in the system is a poor place for a fourth. What makes writing it
defensible is that the tests hand the result to `sgdisk` and `sfdisk` on the build host:
both read back six partitions at the right offsets, aligned, and — the part that matters
— *meaning* the right things, `Linux /usr (x86-64)` and `Linux variable data` from the
Discoverable Partitions Specification rather than sixteen bytes that merely parse.

**Also done:** enumerating disks and refusing the wrong one; copying the running system
across; making `/var`; the `/api/install` route and the console section. The ESP, `/usr`
and the verity tree are copied **byte for byte**, which is possible because both disks
have the same frozen layout — so no `mkfs.vfat` and **no new package in the image at
all**. Every copy is verified by digesting both devices afterwards.

Two refusals are structural. The disk the installer runs from is never offered, found by
resolving the running ESP back to its whole disk rather than by trusting `removable` —
which describes the enclosure and says yes for an internal card reader. And a disk must
have its name typed, because a confirmation that can be clicked through is one that gets
clicked through.

**It has run.** The reference laptop's internal 465 GiB Kingston — which had Windows on
it — was partitioned, written and booted: `/api/install` listed it as `EFI system
partition, Microsoft reserved partition, Basic data partition` so there was no mistaking
whose disk it was, all five refusals were exercised against the real machine first, and
the install took under two minutes. The machine now boots MediaLith from its internal disk
with an empty `/var`, which means a new device token, a new TLS key, and Plex reported as
not provisioned — a fresh appliance, exactly as ADR-0016 intended.

**The label-ambiguity defect the install exposed is fixed**, in both halves and both
proven on the machine — see the first two entries in the trap list.

**The first-boot flow is done too**, and was built while the laptop sat in exactly the
state it is for. It computes rather than remembers: each step is derived from the thing
it is about, so it cannot drift from the machine and cannot be completed by something
that did not happen. Exactly one step is ever "next"; naming the machine and mounting a
share are suggestions, offered in their turn — before Plex, because Plex registers itself
under the name the machine has at the time. Watched moving on its own: writing a
configuration turned one step green and made the next one current, with nothing stored.

**ADR-0016 is Accepted and complete.**

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

**And the appliance now finds its own releases (ADR-0020).** Channels were decided by
nobody: the manifest has carried the field since v1, `sign-bundle.sh` wrote `dev` into every
one, and `[updates].channel` has defaulted to `stable` since the schema was written with no
readers at all — so a machine asking for stable installed development builds without comment.
That is the fourth complete-tested-uncalled design this project has found.

An update service is now files on a web server: `channels/<channel>.json` names a release,
`releases/<release>/` holds the artefacts once and a small signed manifest per channel beside
them. Discovery resolves an address and nothing else — everything after it is the path that
already existed, so the unsigned channel file can only choose which *signed* manifest gets
evaluated.

**All of it has run on the reference laptop, in one sitting**, in this order: the machine
reported itself up to date against a feed; a newer release appeared and it **found it without
anybody typing an address**, showing the release notes and the signing key; it installed from
the service into the inactive slot and the gate made it permanent; a release published only to
`dev` was invisible to the same machine set to `stable`, became visible on `beta` and then on
`stable` as the **same bytes** were promoted (`sha256sum` identical across all three); a
manifest edited after signing was refused; a corrupted `usr.erofs` behind a valid manifest was
refused after the download and before any partition was written; a genuinely signed older
release was refused by the anti-rollback counter with its signer named; and a deliberately
broken release was installed, booted, and **undid itself** — `plexos-0.1.0.202608120253+0-3.efi`
on the ESP, three tries offered and three used, back on the previous release in seven minutes
with nobody at the machine and the update channel and service address still configured
afterwards.

The trap that came out of the last one is in the list below and is the one worth knowing: a
failed update raises the anti-rollback floor before anyone knows it failed.

**And nothing runs unattended any more.** `0.1.0.202607301247` — the first release
installed through the signed path end to end — boots with `plexos-init` still alive as
PID 1. Proved by killing things on the machine: the console shell was killed and came back
with a new pid, Plex was killed and `plexosd` restarted it within twenty-five seconds, and
`plexosd` itself was killed and PID 1 had the console answering again five seconds later.
The last of those is the case worth having built for: Plex survived its parent, was
reparented to PID 1, and the *new* `plexosd` saw it answering and did **not** start a
second server into the same `SQLite` database.

It also found a defect nothing else could have. With PID 1 finally reaping, one zombie
turned up that was not PID 1's to reap: `udhcpc`, a child of `plexosd`. `-b` makes it fork,
so the process `plexosd` spawns exits at once and the resident client is its child — while
a comment in `net.rs` said it stayed resident and there was nothing to wait for. One leaked
process per `plexosd` start, invisible until something else started counting.

**And the other half of rollback has now run too — the one where the image boots and the
system does not work.** A bundle whose Plex could not start (a deliberately missing
`losetup`, the failure this project has had three times for real) was installed twice, and
the two runs are the point:

The first, against the gate as written, restarted to spend a try **twice** and then stopped
— parked on the broken slot announcing that the slot was permanent, with the working system
one restart away and nothing going to ask for it. The bootloader spends a try *to* boot an
entry, so the third boot runs an entry that is already exhausted, and the gate had inferred
which entry booted from the shape of the set rather than asking.

The second, against a gate that asks the running version which entry is its own, went
`+2-1`, `+1-2`, `+0-3`, restart, and came back on the previous slot **with nobody touching
the machine**: 14:07:11 to 14:14:00, three restarts, ending on `0.1.0.202607301330`. The
record left on `/var` carries `tries_left: 0` — the value `rollback.rs` describes as "the
one that changed slots" and that no machine had ever written.

**The console is on TLS, and `http://` no longer serves anything.** The appliance issues
its own certificate — there is no CA and no domain name — and 80 answers a 308 to 443 and
nothing else. Proved on the reference laptop: the certificate it presents carries
`IP Address:192.168.2.102` (an address recorded as a DNS name is one no browser matches),
is valid 1975 to 4096 so a dead RTC cannot make it not-yet-valid, and the fingerprint at
`/api/status` is the same key `openssl s_client` sees on the wire — computed two
independent ways and compared.

Two things it does not claim. It stops anyone *listening*; against an active middle a
self-signed certificate proves nothing until somebody compares the fingerprint, and the
only place to do that the first time is the attached screen — which is what this console
exists to stop needing. ADR-0014 called that unresolved and it stays unresolved. And the
choice to serve TLS *only* was taken knowing the cost: if TLS ever fails to start, the
console is gone and the ways back are the screen or three power cycles.

**Revocation has run too, and with it every part of ADR-0006.** No image was rebuilt: the
code shipped with signing and this was the first time anything used it.

`plexos-sign revoke <root-key> 1 plexos-signing-dev` published beside the manifest. The
appliance fetched it, verified it against the root key, stored it on `/var`, and then
refused the manifest it had accepted an hour earlier — *"signed by plexos-signing-dev,
which has been revoked… this update must not be installed even though its signature is
valid"*.

Then the half that makes it a rotation rather than a brick: a second signing key, certified
by the same root, was used to re-sign the same bundle and **the appliance accepted it with
no OS update**. That is what the two tiers are for, and it had never been demonstrated.

And the counter: an older list — genuinely root-signed, counter 0, revoking nothing —
served in place of the real one changed nothing. The revoked key stayed refused. Replaying
a pre-revocation list is the obvious attack the moment revocation exists, and it does not
work.

`/var/lib/plexos/update/` now holds `accepted_sequence`, `revocations.json` and
`rollback.json`, all three written by the machine itself. None of the three is a constant
without callers any more, which is where two of them started. **Kernel images are still unsigned, so Secure
Boot must be off** — that is ADR-0004 and separate from update signing.

## What exists, in one place

An index, so nothing here has to be rediscovered by reading the tree.

**The attached screen** — `crates/plexosd/src/dashboard/`, a thread inside `plexosd` that
owns `/dev/tty1`. What the machine is, whether it works, and the address to point a browser
at; **P** puts a QR code on it that signs a browser in for twelve hours. The model, the
rendering and the QR are three modules so that every state — recovered, on trial, no
network, pairing, expired — is a test rather than a photograph. `plexos-init` moved the log
and the console shell to `/dev/tty2`, one **Alt+F2** away, because a log written over a
drawing wins.

**The console page** — one file, `crates/plexosd/src/ui/console.html`, embedded with
`include_str!`. No framework, no build step, no external anything. Since the redesign it is
an application shell rather than a long page: a sticky header (what the machine is, the
health verdict, the address, the slot, uptime, the administrator lock, restart and shut
down), a sidebar of seven views — Overview, Plex, Storage, Network, System, Events, and
Terminal under Advanced — and one view showing at a time.

The Overview's rail leads with **Now Playing** (ADR-0018), and the Plex view lists every
stream in full. Both are blank and **fetch nothing** without the device token: a page that
downloaded the titles and declined to draw them would have put them in a browser that was
never entitled to them.

Nothing navigates. Switching is `hidden` on six sections and off one, so the six polls, the
terminal's scrollback and the typed token all survive a click; `pushState` and `popstate`
keep the URL and the view agreeing, so `#network` is a link somebody can send and the back
button works. The device token is behind the lock in the header rather than in a card at the
top of the page, and **ADR-0013 is untouched by that**: same field, `sessionStorage` only,
same `Authorization: Bearer` header. Light/Dark/System is kept in `localStorage`, because it
is a property of the browser looking rather than of the machine being looked at.

The rules the page is built around, each of which has cost a fault: nothing anybody types
into may live inside a region a poll replaces; anything with state a person set — an open
disclosure, a table they asked for — may not either; a failure must be shown in the section
whose button caused it, and must open that section if it is folded; and a `GET` stays
readable without a credential so a broken machine can still be diagnosed.

**Console API** — HTTPS only, port 443; port 80 answers a 308. `GET` needs no credential,
every `POST` needs an administrator credential, and the terminal is *all* `POST` so a root
shell's output cannot be read without one. **Two credentials are accepted and no route knows
which arrived**: the recovery device code (ADR-0013) and an administrator session issued by
pairing at the machine's own screen (ADR-0019). `auth::authenticate` is the only thing that
decides, and `http::refusal` is the only thing that calls it.

| Route | What it is |
| --- | --- |
| `/api/status` | Image identity, slot, root hash, whole kernel command line, health checks, TLS fingerprint |
| `/api/metrics` | What the machine is doing now: processor per core, memory, Plex's own cgroup, GPU clock, temperatures, free space, throughput. Rates, not since-boot totals |
| `/api/metrics/processes` | What is running. **A `POST`**, so the method-based gate applies: a process list with command lines is not something every reader on the LAN should have |
| `/api/plex/sessions` | What Plex is playing now: who, on what, and what is happening to the picture and the sound (ADR-0018). **A `POST`** for the same reason and a stronger one — a title, a username and a device name are what somebody in the house is doing this evening |
| `/api/setup` | The first-boot flow: ordered steps, computed not stored (ADR-0016) |
| `/api/install` | Disks, refusals, and installing MediaLith onto one (ADR-0016) |
| `/api/update` | Check and install a signed update; gate verdict; rollback record (ADR-0005/0006). A body with no `source` means "the release the update service offered" |
| `/api/update/check` | Ask the configured update service what it has, and install nothing (ADR-0020) |
| `/api/provision` | Install Plex from Plex's own packages (ADR-0010) |
| `/api/config`, `/api/network` | Hostname, timezone, static addressing (ADR-0008) |
| `/api/shares` | Network shares the library lives on |
| `/api/terminal` | Root shell, long-polled (ADR-0014) |
| `/api/power` | Shut down, restart |
| `/api/pair` | Spend a pairing code for an administrator session (ADR-0019). **The one mutating route with no credential**, because it issues one — and it has nothing to spend unless somebody pressed P at the machine |
| `/api/session` | Is this browser still an administrator, and sign it out |
| `/api/browser-pair/*` | One authorised browser approving another (ADR-0019). `start`, `redeem` and `cancel` carry no credential — the first because asking is not being let in, the other two because they need a secret only the asking browser has. `inspect`, `approve` and `deny` are gated like everything else |
| `/api/token` | Rotate the recovery device code. Revokes every session and any pairing offer |

**Publisher tooling**, all on the build host:

| Tool | What it does |
| --- | --- |
| `tools/sign-bundle.sh` | Turns a built bundle into `manifest.json` + `.sig`, then verifies it with the appliance's own verifier |
| `tools/publish-update.sh` | Serves the bundle; refuses one with no signed manifest |
| `tools/break-bundle.sh` | Corrupts an image and re-signs it, to exercise ADR-0005 |
| `tools/publish-release.sh` | Puts a signed bundle into a static update tree an appliance can poll (ADR-0020) |
| `tools/promote-release.sh` | Publishes the *same bytes* to another channel; refuses if any digest moved |
| `tools/publish-revocations.sh` | Copies a root-signed revocation list into every release directory |
| `plexos-sign` | `root-key`, `signing-key`, `certify`, `sign`, `check`, `revoke`, `trust` |

**State on `/var`** — the only surface a rollback leaves alone, which is why every one of
these is here and not in `/usr`:

| Path | Written by |
| --- | --- |
| `update/accepted_sequence` | The anti-rollback floor (ADR-0006) |
| `update/revocations.json` | The root-signed revocation list in force |
| `update/rollback.json` | Why a boot was handed back to the other slot (ADR-0005) |
| `tls/` | The console's key and certificate; the key outlives the certificate |
| `apps/plex/` | Plex app images and the `current` link (ADR-0007) |
| `etc/` | The writable half of the `/etc` overlay |
| `STATE_VERSION` | The `/var` layout version (ADR-0009) |

**Keys** live in `~/.plexos-keys/` on the build host, outside the repository: `root-dev`
(baked into `ROOT_KEYS` as a *development* key), `signing-dev` (**revoked**), `signing-dev-2`
and its certificate — sign with the second one.

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
- **The same trap again, in the kernel fragment, and it hid for months behind evidence
  pointing somewhere else.** `CONFIG_FONT_TER16x32` and `CONFIG_FONT_TER10x18`
  `depends on !SPARC && FONTS`, and `CONFIG_FONTS` was never set — so both were dropped
  in silence and the image shipped with the two fonts the kernel picks on its own. The
  command line then asked for `fbcon=font:TER16x32`, `find_font` returned NULL for a font
  that had never been compiled in, and fbcon carried on with 8x16. On the reference
  laptop's 2880x1620 panel that is a 360x101 grid: characters about three millimetres
  tall on a fifteen-inch screen, reported by the person looking at it as "the text is
  barely visible".
  Everything about the symptom says *the command line does not take effect*, which is
  where two attempts to diagnose it went. The fault is a Kconfig dependency two
  directories away. **`ls output/build/linux-*/lib/fonts/*.o` answers it in a second** —
  two object files where there should be four — and that is the shape of check worth
  reaching for whenever a `CONFIG_*` symbol appears not to have done anything: look for
  what the build *produced*, not for what the configuration says.
  `plexos_sys::tty::use_font` now asks the kernel directly as well, so a font that is not
  there fails in a log line naming both symbols instead of silently.
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
  in `rootfs.erofs` and absent from the `/usr` image MediaLith actually boots. `post-image.sh`
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
- **`xe` says the same things as `i915` in different words, and in a different place.**
  Two halves, both silent. The path: `xe_gt_debugfs_register()` creates `gt<N>/` under the
  *tile*, and `dri/<N>/gt<N>` exists only as a symlink whose comment in the kernel reads
  "Backwards compatibility only … for the legacy clients". Reading through it works today
  and rests on a shim already labelled legacy. The vocabulary: a HuC that failed to come
  up is `LOAD FAIL` from `xe_uc_fw_status_repr()`, never the `authenticated: no` that
  `i915` prints, and a part with no HuC at all is `N/A`, not `not supported`. A reader
  written for one driver's words returns `Unknown` for the other's — and `Unknown` is
  deliberately excluded from `has_confirmed_problem()`, so the failure this crate exists
  to catch would have been reported as "cannot tell, debugfs is probably not mounted"
  about a file it had just read. Both halves came out of the kernel source rather than a
  capture, because there is no `xe` hardware here to capture from.
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
- **Git refuses a push to the branch the build host has checked out, and a build will
  happily carry on without it.** `receive.denyCurrentBranch` rejects it — correctly — but the
  refusal is on the *push*, several steps before the build, and if that output goes anywhere
  nobody reads then `make` runs against the previous sources and stamps them with the new
  version. That is the trap below wearing a different coat, and it survives `plexosd-rebuild`
  completely: the rebuild was genuine, it just rebuilt the old file. Two things fix it, and
  the second matters more than the first: **push to a ref nothing has checked out** and
  `git reset --hard` onto it, and **grep the build host's checkout for something the change
  added, before spending a build on it.** Found by comparing the SHA-256 of the page the
  appliance serves against the working tree's — which is the check worth keeping, because it
  compares the artefact to the source and nothing in between can lie about it.
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
- **A Buildroot firmware symbol's number is not the generation it sounds like, and the
  wrong one is silent in both directions.** The same Alder Lake laptop, this time with no
  `wlan0` at all, got `BR2_PACKAGE_LINUX_FIRMWARE_IWLWIFI_22000` — which reads like the
  modern family and installs `iwlwifi-Qu-*` and `iwlwifi-QuZ-*` alone. AX210 and AX211 are
  `iwlwifi-{so,ty}-a0-gf-a0*` and come from `IWLWIFI_6E`; `22260` is `cc-a0`. So the image
  gained 53 MiB of firmware for a card that is not fitted and the machine came up exactly
  as before — a fix that is expensive, plausible and inert. Nothing names the product
  anywhere: the files are named for the silicon, so `AX211` is not greppable in
  `linux-firmware` or in Buildroot. **Read `package/linux-firmware/linux-firmware.mk` for
  what a symbol installs**, the way `CONFIG_*` sub-symbols already have to be read.
- **Shipping every API revision is most of the size and none of the benefit.** iwlwifi
  asks for one revision per device and counts down to its minimum — and for every family
  in this image `IWL_*_UCODE_API_MIN` *equals* its `_MAX`, so exactly one file per variant
  is ever opened while `linux-firmware` ships seven of each Qu part and thirteen of
  `ty-a0-gf-a0`. Keeping the newest per variant took the wireless set from 70 MiB to
  17 MiB *while covering more cards*, and the UKI from 112 MiB to 59 MiB — against the
  128 MiB `partition.rs` budgets and three of them on a 512 MiB ESP. A firmware list is
  charged four times: initrd, both UKIs, every bundle.
- **The AX210 family needs a `.pnvm`, which is not a `.ucode`.** A glob written when the
  image carried only 9000-series firmware ships an AX211 that loads its firmware and
  associates with nothing — the missing-file failure one directory further on, wearing a
  firmware directory that is visibly not empty.
- **Stage 3 of `post-image.sh` had no test at all**, which is how all three of the above
  passed a build, a boot and a clean run of `post-image-test.sh` reporting 47 checks and
  no skips. A stage nothing exercises reports nothing when it is wrong; count the stages
  against the tests rather than trusting the total.
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
- **Everything in MediaLith resolves partitions by label, and an installer makes labels
  ambiguous — including for the updater and for PID 1.** This is bigger than the console
  bug below it and was found the same evening. With MediaLith installed to an internal disk and
  the USB stick still plugged in, the machine has two partitions called `esp`, two called
  `usr_a`, two called `var`. `plexos_sys::device::by_partlabel` returns whichever the kernel
  enumerated first, and it is used by the updater to choose the partition to *write*, by
  `esp::with_esp_mounted` to choose the ESP to install a boot entry on, and by `plexos-init`
  to choose what to mount as `/usr` and `/var` at boot. An update installed in that state
  went to a disk nothing in the code chose, and the evidence is on the ESPs: the machine was
  running from the USB stick, and both the `/usr` write *and* the boot entry landed on the
  internal disk. The stick's ESP never saw the new entry. It was harmless and it was not a
  decision. **Label resolution has to be scoped to the disk the running system is on**, and until
  it is, a machine with two MediaLith disks attached is a machine whose updates land somewhere
  arbitrary. **Fixed** by `by_partlabel_on(disk, label)` and a running-disk lookup that goes
  through dm-verity's `slaves` rather than through a label — in the updater's partition
  writes, the boot-entry install, the installer's own source resolution, and the health
  gate, which clears the try counter and would otherwise have cleared it on another disk's
  ESP: a working machine rolling back three boots later for no reason it could report.
  **Proven on the machine**: with both disks attached, an update written from the internal
  system landed on the internal disk and the stick's ESP was left exactly as it was —
  where the same operation an hour earlier had written the other disk. **Both halves are now closed.**
  `plexos-init` cannot use the same lookup — dm-verity is what it is in the middle of
  setting up — so it asks the only thing that knows: `systemd-boot` writes the partition
  GUID of the ESP the firmware loaded into `LoaderDevicePartUUID`, and the kernel already
  puts `PARTUUID` in every partition's `uevent`. No extra tool, no GPT parsing, and the
  answer is authoritative rather than inferred. Failing to establish it is *not* a failed
  boot: it falls back to resolving by label, which is what every one-disk machine has always
  done. **Proven with both disks attached and every label duplicated**: `plexos.slot=a`,
  `/usr` backed by `nvme0n1p2` and `nvme0n1p3`, `/var` on `nvme0n1p6` — all on the disk the
  firmware booted, where each of the three had previously been a coin toss.
- **Partition labels are not unique across disks, and an installer is what makes that
  true.** The console found the disk it was running from by resolving the ESP's partition
  label. That worked until the first successful install, at which point the machine had two
  partitions called `esp` and `by_partlabel` returned the one on the disk that had just been
  written — so the console reported MediaLith as running from the *target*, and would then have
  offered the disk it was actually running from as somewhere to install. Accepting that
  erases the running system. The copy path had been designed against exactly this hazard,
  by resolving the source partitions before writing anything; the same hazard in a second
  place was missed and found on hardware a minute after the install succeeded. The answer
  is not to guess from labels: the verified `/usr` is a device-mapper device and sysfs lists
  the real partitions behind it under `slaves/`.
- **"I do not know" and "nothing is excluded" are the same value and opposite meanings.**
  `running_disk` returning `None` had to become a refusal of every disk rather than an
  install with nothing ruled out.
- **The bootloader spends a try *to* boot an entry, so the entry running can already be
  exhausted.** The gate inferred which entry had booted from the shape of the set — none
  counting plus a permanent one present meant the permanent one was running — and that is
  wrong exactly once, on the third boot of a bad update. `systemd-boot` picked the bad entry
  while it still had one try, decremented it to `+0-3`, and booted it; the gate then filtered
  the exhausted entry out, saw the good permanent one, and announced "this slot is already
  permanent" about a slot on trial. Two restarts, then a machine parked on a broken system
  with the working one a single reboot away and nothing to ask for it. **The running version
  names its own entry**, so nothing needs inferring: `os-release` is inside the dm-verity
  `/usr` this boot mounted, which makes it a better answer to "what booted" than anything on
  a FAT partition.
- **Every successful update left its predecessor's boot entry, and 25 of them filled the
  ESP.** `install_entry` never removes the entry that works — right, it is the way back —
  and nothing removed the ones before it. The reference laptop reached 25 entries on a
  511 MB partition at 100% full, and the update that found it failed with `ENOSPC` halfway
  through copying a kernel, leaving a truncated 664 KB file named `+3`: the highest version
  on the partition with a full try counter, so the bootloader would have chosen it first
  and spent three boots discovering it was not a kernel. The rule that fixes it comes from
  the architecture rather than from a retention count: **there are two slots, so after the
  partitions are written the disk holds exactly two versions of `/usr`**, and an entry
  naming any other version is guaranteed not to boot. Prune before installing, not after,
  because the failure being prevented is running out of room during the install.
- **There is an unreproduced flake in the suite, seen three times.** A single `cargo test
  --workspace` failed once with `plexos-plex`'s `a_mkfs_without_the_compressor` and twice
  with nothing captured, against **more than fifty** deliberate consecutive runs that were
  clean — so roughly one in twenty-five, and it has never been caught with output. The
  suspect is the shape below: that test writes a shell stub to a fixed path and then
  executes it, which races with any other test in the same process that forks — `ETXTBSY` on
  a file another thread holds open for writing. Recorded rather than guessed at, because a
  speculative fix to a test that cannot be made to fail is a change nobody can check.
- **A shared scratch path makes two passing tests into one flaky pair.** Rust runs tests as
  threads in one process, so a fixed temp file is a race: one test deletes what another is
  reading and they fail in whichever order the scheduler picked. Hit twice in one day —
  once written fresh in the TLS tests, once suspected in `plexos-plex`'s `mkfs` stub, which
  writes and then executes a script at a fixed path and failed once under load and never
  again. Same shape as the `waitpid(-1)` collision below: the hazard is the shared process,
  not the code under test.
- **A script that addresses an element nothing creates is a silent hole, and no assertion
  about text can see it.** The setup section shipped with its markup never added: the script
  called `getElementById` for it, got `null`, threw, and the throw was swallowed by the
  poll's own error handling — endpoint fine, page fine, feature simply absent. The tests
  passed because they asserted that strings appear in the page and those strings were in the
  *script*. Second time in one day that a page change reached a machine unverified, in a
  weaker disguise. `every_element_the_script_reaches_for_exists_in_the_markup` compares
  every `getElementById` against the markup, and found two more instances the moment it was
  written — both legitimate, created by `element.id = …`, which is now part of what it
  accepts.
- **The console is one inline script, so a syntax error anywhere stops the entire page.**
  Every section sits on "Loading..." while the API answers perfectly, which reads as a dead
  machine and is a dead *page*. It shipped from a second `const signature` inside a
  function that already had one: a duplicate `const` is a **parse** error, so nothing ran
  at all — not even the sections that had nothing to do with the change. Nothing in this
  repository had ever parsed that file; the page's tests assert that strings appear in it,
  which a completely broken script satisfies. `the_pages_script_parses` runs `node --check`
  now, and announces a skip when no engine is installed, because a check nobody knows was
  skipped is a check nobody has. It has since earned its keep on a second flavour of the same
  thing: **a backtick inside a template literal ends the literal**, and the offender was an
  HTML *comment* — one explaining why a variable existed, written as `` `lastNetdiag` ``,
  inside the very template it was describing. Prose is code in there; quote code in comments
  with nothing, or with quotation marks.
- **A name used twice on the console page is not an error anywhere, and both halves of the
  duplicate keep working — on the wrong thing.** The Plex card's button and the disk
  installer's section were both `id="install"`, and both click handlers were
  `function startInstall`. `getElementById` answers with whichever element the markup puts
  first, which was the button; a repeated function declaration silently replaces the earlier
  one. So `renderInstall` wrote the whole "Install to a disk" card *into the Install Plex
  button*, the real section stayed on "Loading...", and the only action the page offered a
  freshly-installed appliance was erasing a disk — with the button's own handler now pointing
  at the installer, so one click sent the request twice. Found by a person looking at the
  served page and asking what there was to press, on a machine where the previous two checks
  both passed: every element the script addressed existed, and the script parsed. The
  distinguishing question is not "does this name resolve" but "does it resolve to *one*
  thing". `no_id_is_given_to_two_elements` and `the_script_declares_no_function_twice` ask
  it; note that only the `const` flavour of this is a parse error, which is why
  `the_pages_script_parses` could not see the `function` one.
- **CSS specificity is the console's silent failure mode, and it has now cost four
  faults in one afternoon.** All four passed every test in this repository, because every
  test here reads the page as *text* and in all four cases the text was right. Only a
  rendered page shows them.
  - `#view-overview { display: flex }` beats `.view[hidden] { display: none }`, because an
    id outranks any number of classes. So the Overview stayed on screen underneath every
    other view, with the sticky header floating halfway down a page of summary cards. **An
    id in a layout rule needs the state in the selector**: `#view-overview:not([hidden])`.
    Already recorded once on `#terminal.folded`, and walked into again.
  - `.navitem` is one class and loses to `button:not(.ghost)`, which is an element and a
    class — so the sidebar rendered as seven solid blue buttons, which is exactly what the
    base rule guarantees a button looks like. `button.navitem`, placed *after* the base
    rule, wins on equal specificity plus source order **without naming where the button
    is**, which is the property `buttons_are_styled_without_naming_where_they_are` exists
    to keep.
  - The same again for `.linkish`, and the colour was the half that was missed: a
    cross-reference inside a muted paragraph rendered white on nearly-white and could not
    be seen at all.
  - Two media queries both apply on a phone, because a phone is narrower than a tablet. The
    tablet rail hides nav labels with `button.navitem .label`; the phone rule bringing them
    back was written `.navitem .label`, one class less specific, so it lost however far
    down the sheet it sat. The drawer opened to a column of seven unlabelled icons — a
    rail, not a drawer, and the whole point of a drawer is that there is room for words.
- **A page wider than its window is cropped in a screenshot, which looks exactly like a
  page that fits.** "No horizontal scrolling" cannot be checked by looking. The System view
  was 54 px wider than a 390 px window from one `white-space: nowrap` pill, and six
  screenshots of that view had already been read as fine. Measure it in the browser —
  `documentElement.scrollWidth` against `clientWidth`, per view — and have the probe **name
  the widest element that sticks out**, because "something overflows" is not something
  anybody can act on. `tools/preview-console.py` serves the page; a few injected lines do
  the rest.
- **A rename applied to a file rather than to an identifier rewrites string literals with
  it.** `metricLevel` returned `warn-metricLevel` and `bad-metricLevel` while the stylesheet
  defined `.meter.warn-level` and `.meter.bad-level`, so **no meter on the activity card had
  ever left the accent colour**: a `/var` at 96% full drew exactly like one at 6%, which is
  the one reading that card exists to make loud. Both halves stayed internally consistent,
  so nothing reading the page as text could see it. The test that catches it takes the class
  names from the function *by running it* and looks each one up in the stylesheet — which is
  the general shape for any pair of files that have to agree on a name.
- **`\b` is a backspace only inside a character class, and the terminal was deleting the
  last character of every word.** `body.replace(/[^\n\b]\b/g, "")` was meant to say "drop a
  character that a backspace erased". The `\b` *inside* the class is `\x08`, so the first
  half says exactly what it looks like; the one *outside* is a **word boundary**, so the
  rule actually said "drop the last character of every word". `total 4` rendered as `tota`,
  `root` as `roo`, `drwxr-xr-x` as `drwxx` — and replaying a captured session through it
  showed **1371 characters of output becoming 541**, for the whole life of the feature.
  Nothing here could see it: the page's tests assert what is *in* the page, and the page
  was fine — it was the behaviour that was wrong. Reported from a machine by somebody
  reading a shell's output, twice, and the first report was misdiagnosed here as short
  lines in a wide box, because the box and the PTY were measured and the renderer was not.
  Two lessons. Spell control characters `\x08`, `\x1b`, `\x0d`, never `\b`/`\e`/`\r` in a
  pattern where a class boundary changes the meaning. And **a transform is testable only if
  it is a function**: `termClean` is pure and separate from the element it writes into, so
  `the_terminal_renders_what_the_shell_printed_and_not_less` can run the page's own code
  under `node` over chunks split the way a poll splits them — mid escape sequence, and
  between the two halves of a CRLF, both of which were also broken and neither of which
  any assertion about text could have found.
- **This appliance restarts itself by design, and two of Plex's files do not survive being
  interrupted mid-write.** A zero-length `Preferences.xml` — Plex truncates and rewrites it,
  so a stop in between empties it — makes Plex log `Failed to load preferences` and never
  open its port; it will not replace an empty one with defaults. A stale
  `plexmediaserver.pid` makes it start and *wait*: alive, two threads, twenty-six megabytes,
  nothing listening. Either fails the boot health gate, which restarts to spend a try, which
  is another unclean stop. **A rollback cannot cure either, because the fault is on `/var`
  and `/var` is exactly what a rollback leaves alone** — the reference laptop spent both
  tries and came back on the previous release still wedged, which is how this was found and
  is the sharpest demonstration yet of what that rule costs. `plex::clear_wedged_state`
  removes both before every start, and touches nothing that has anything in it. The gate's
  own behaviour was right throughout: it stopped restarting once the entry was permanent,
  saying that looping would take away the console, which is the only reason the machine was
  still diagnosable.
- **A render cache keyed on the wrong fields hides the field you just added.** The update
  section redraws only when one of a listed set of values changes, and a newly-verified
  signature is the only thing that changes when a check runs — so the "Signed by" line
  would have appeared on the next unrelated redraw and looked intermittent. Adding a field
  to a page means adding it to whatever decides the page is stale.
- **A daemon that spawns and never waits leaks a zombie per spawn, and only PID 1 reaping
  makes it visible.** `plexosd` starts `udhcpc` with `-b`, which forks — so the process it
  spawned exits at once and the resident client is a *grandchild*. The comment in `net.rs`
  said udhcpc stayed resident and there was nothing to wait for, which describes the
  behaviour without `-b`. One leaked process per `plexosd` start, found on the appliance the
  hour PID 1 began reaping, because a zombie turned up whose parent was not PID 1. The fix
  is `Child::wait` on a thread and **not** a general reaper: `waitpid(-1)` in a process that
  also runs `Command::output()` steals the child that call is waiting for.
- **`waitpid(-1)` is process-wide, so only PID 1 may reap.** It collects *any* child,
  including one something else in the same process is waiting for. The first thing it broke
  was the test suite: Rust runs tests as threads in one process, so the new reaping tests
  stole the PTY tests' children and each failed depending on the scheduler. The same hazard
  applies to any library that reaps indiscriminately inside a process that also uses
  `Command::status()` — that call then hangs, or reports a status belonging to something
  else. `plexos-sys` serialises the tests that spawn and says why in `CHILD_PROCESS_TESTS`.
- **A supervisor that watches one signal watches the wrong one.** "Is my child alive" is
  false for a perfectly good Plex orphaned onto PID 1 when `plexosd` restarted; "is the port
  answering" is false for twenty seconds every time Plex starts. Watching either alone
  produces a specific disaster — the second server started into the same `SQLite` database
  — so `restart_reason` consults both, plus whether Plex is *meant* to be running, which is
  what stops it fighting the shutdown sequence.
- **Two outcomes that both do nothing still need different words.** A check that found a
  newer release and a check that found none took the same `Ok(None)` out of the updater, so
  the page said "already up to date" directly underneath a line naming the version it had
  just found. Both are true statements about the machine and only one answers the question
  that was asked. Found in the first minute of driving the signed path on the appliance,
  having survived every test, because a test asserts what a function returns and this was a
  defect in what the return *meant*.
- **A schema written before the artefact exists describes an artefact that does not
  exist.** `plexos-types::manifest` had one `uki` field and one `os_version` of the form
  `MAJOR.MINOR.PATCH`. MediaLith builds two UKIs, because `plexos.slot=` is on the command
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
- **The console cannot be measured through the console.** The screen blanks after five idle
  minutes now, and every attempt to *observe* that through `POST /api/terminal` failed —
  because opening a session makes `plexosd` log a line, PID 1's log goes to the console, and
  console output both unblanks the screen and resets the blank timer. So the act of looking
  put the screen back on, three different sampling harnesses in a row measured a lit panel and
  looked like a broken feature, and the thing that settled it was Sebastian saying "102 zgasł
  ekran". Two lessons: a detached sampler does not survive `take_over: true`, which replaces
  the session and kills its children; and when the quantity is *what somebody sees*, the
  person in front of the panel is the instrument, not a workaround for the lack of one.
  Incidentally `/sys/class/graphics/fb0/blank` is not that instrument either — it read `4`
  while `actual_brightness` was at maximum. `actual_brightness` and the connector's `dpms` are
  the honest signals.
- **A region on a timer destroys anything a person opened inside it.** The activity card
  redraws twice a second, and the process table and the "what these numbers do not say"
  section were rendered into that same element — so both worked for up to two seconds and then
  shut themselves, which reads as a control that refuses to stay open. Reported from the
  machine within minutes of the card being installed. The fix is not a saved-and-restored
  flag: the stateful parts are written once in the markup as **siblings** of the redrawn
  region, and only their contents are updated. This is the "correct in the state it was
  written in" trap again, and the new detail worth keeping is *when* it appears — the card had
  no state at all until it grew a table and a `<details>`, so the polling loop was harmless
  right up to the commit that made it not.
- **A style rule that enumerates the kinds of a thing will miss one — and narrowing the list
  is not the fix.** Buttons were styled by `.plex button`, `.form button` and `.power button`,
  so the network card — a plain `<div class="card">` — matched none of them and its "Diagnose
  the network" button had **no styling at all**: a raw operating-system control in the middle
  of a designed page, shipped and reported by somebody looking at it. The stylesheet already
  carried a comment saying the rule had been written three times and that the common half had
  been factored out; the selectors stayed enumerated, so the gap survived the tidy-up that was
  about it. That was then "fixed" to `.card button` — which missed the next button added, the
  power controls in the sticky header, **the same trap one turn of the screw later**. The rule
  is on the element now: a rule about how a button looks should not know where buttons are.
  Ask what a rule does about a case that does not exist yet.
- **A presence check can be satisfied by a string that asserts the opposite.** The build
  script greps the rsynced package tree to prove the new code got in. One marker was
  `card button {` — and it passed *after that rule had been deleted*, because `grep -r` found
  it in the list of forbidden selectors inside the test asserting its absence. A check that
  cannot fail is worse than no check, because it is counted. Grep the **artefact** — the page,
  the module — not the tree that also contains everything written *about* it.
- **A function that names itself in its own output cannot be composed.** `humanUptime`
  returned `"Up 1 h 21 min"` while its own comment described only the duration, so both callers
  added a label of their own: the header badge read **"UP UP 18 MIN"** and the activity tile
  was headed "Up" above a value of "Up 1 h 21 min", which had been on a machine for a day
  without anyone noticing. A formatter returns the value; the caller says what it is.
- **A class name in the page's script is a channel nothing was watching.** Three tests guard
  the console page — its script parses, every id it addresses exists, no id names two things
  — and a rename that mangled `class="meter"` into `class="metricMeter"` passed all three.
  The page would have rendered every element, addressed every id, parsed cleanly, and drawn
  as unstyled text. `every_class_the_script_draws_with_is_a_class_the_stylesheet_defines` asks
  the fourth question, and its rule is "styled, **or** selected on" rather than "styled",
  because some classes exist to be found rather than painted — it flagged `media-pick` and
  `share-drop` on its first run, both legitimate `querySelectorAll` hooks. A list of
  exceptions would have gone stale; a property of the page does not.
- **`preserveAspectRatio="none"` scales shapes, and `vector-effect` only exempts the
  stroke.** A sparkline stretched to a tile's width has an x scale about twice its y scale, so
  the `r="3"` end marker came out a six-pixel-wide, three-pixel-tall smear — and at `cx="100"`
  half of it fell outside the viewBox and was clipped by the tile edge. Marking the current
  value with the last *segment* in the accent is immune, because a stroke is all it is. Found
  by rendering the card and looking at it; both the function's output and `node --check` were
  perfectly happy.
- **A sparkline's x axis must span the points there are, not the ring's capacity.** Divided by
  a sixty-sample ring, four samples drew a line five per cent of the tile wide tucked in a
  corner — which reads as a rendering fault rather than as "not much history yet". It shipped
  twice: once anchored left, then "fixed" by anchoring right, which moved the same stub to the
  other corner. `the_sparkline_spans_the_tile_whatever_it_has_to_draw` runs the page's own
  function under `node`, the way the terminal cleaner's test does, and both versions fail it.
- **A thermal zone's `type` is not unique.** A development host answered with two zones both
  typed `acpitz`, at 27.8 °C and 29.8 °C, so a table keyed on the type showed one label twice
  and no way to attribute either reading. The `thermal_zoneN` directory is what separates
  them. Same question as the duplicated `id` on the console page: not whether a name resolves,
  but whether it resolves to *one* thing.
- **`coretemp` is the hwmon driver; `x86_pkg_temp` is the thermal zone.** Anything reading
  `/sys/class/thermal` and reporting "no processor temperature, enable
  `CONFIG_SENSORS_CORETEMP`" has named the wrong symbol —
  `CONFIG_X86_PKG_TEMP_THERMAL` is what publishes the zone, and it has to be `=y` here
  because `CONFIG_MODULES` is off. Both spellings were checked against a real
  `/boot/config-*` rather than recalled.
- **Two tests in the suite fail on any development host running Plex.**
  `plex::tests::a_handle_that_has_started_nothing_reports_nothing_running` and
  `an_unprovisioned_machine_is_told_where_plex_would_be_rather_than_failing` both go through
  `Handle::is_running`, which probes `127.0.0.1:32400` — correctly, since ADR-0005's gate has
  to ask the port rather than only its own child. On a machine that has its own Plex the probe
  succeeds, so a clean `cargo test --workspace` is impossible there and CI's green run says
  nothing about it. Not a defect in the code under test; a suite that is not hermetic.
- **Plex's hardware-transcoding fields are empty until the transcoder actually runs, and
  empty looks exactly like software.** A session captured seconds after it started reported
  `transcodeHwRequested: false` and `transcodeHwDecodingTitle: "Intel ()"` — the parentheses
  hold the API name and are empty. The *same session* moments later reported
  `transcodeHwDecoding: "vaapi"`, `transcodeHwEncoding: "vaapi"` and
  `transcodeHwFullPipeline: true`. So the only positive evidence is a named decoder or
  encoder, and "no evidence" means software **only** once the transcoder has demonstrably
  produced something (`progress > 0`). Reading the first state as software puts an amber
  warning on this appliance's whole reason for existing, every time somebody presses play.
  `Video::hardware` is an `Option` for this, and `None` is a third answer rather than a
  missing one.
- **Plex says nothing at all about a Direct Play, so it has to be read out of the silence.**
  A direct-play session captured from the appliance had no `TranscodeSession` node and no
  `decision` field anywhere in it — not `"decision": "directplay"`, nothing. The absence is
  the answer. `Decision::default()` is therefore `Unknown` and not `DirectPlay`, so the best
  case cannot be arrived at by a field failing to parse.
- **A transcoding session's `Media` node is the *output*, so the file's own resolution is
  not in it.** `videoResolution` read `720p` and `container` `mpegts` for a 4K MKV. The
  source codec is in `TranscodeSession.sourceVideoCodec`; the source resolution and the HDR
  format exist only in `/library/metadata/<ratingKey>`, which is why `plexactivity` looks
  them up and caches them. The one place the session *does* carry the source is a display
  string — `"4K DoVi/HDR10"` — which is composed for people and partly localised, so parsing
  it would be the fixture-you-imagined trap in a new coat.
- **A hardware verdict belongs to a video transcode and to nothing else.** Found on the
  appliance on a real Direct Stream — video copied, audio re-encoded — where the transcode
  session had progressed and named no decoder, so "has it named hardware" answered `false`
  about a picture nothing was decoding. The page does not draw it in that state, which is
  what made it invisible: a field that is wrong only where nothing reads it is a field that
  becomes wrong somewhere that does.
- **A failed update raises the anti-rollback floor before it is known to be failed, so the
  release you were running becomes uninstallable.** The sequence is recorded when the boot
  entry is installed — deliberately, because recording it earlier would let one failed
  download refuse a release for ever. The sibling case had never been run: install a broken
  release, let ADR-0005 hand the machine back, and the floor now stands *above* the release
  the machine is running. Republishing the previous good release — the obvious recovery
  instinct — is refused as a downgrade, correctly and unhelpfully. Nothing is bricked and
  the console works; the remedy is to publish a **higher** stamp, which any new build is.
  Made worse here by the experiment itself: `break-bundle.sh` takes a version, and stamping
  the broken bundle thirty minutes into the future put the floor above real time, so the
  next genuine build was refused too. **Stamp a deliberately broken bundle one minute above
  the running release, never into the future.**
- **A tool that calls another tool is broken by that tool gaining a required argument, and
  nothing says so until somebody reaches for it.** `sign-bundle.sh` gained a mandatory
  `--channel`; `break-bundle.sh` calls it and would have failed at the moment somebody was
  half-way through a rollback experiment. Grep for callers of a script when changing its
  interface, exactly as for a constant.
- **A test that spawns must both hold the lock and collect its own child.** Four `pty` tests
  forked without `CHILD_PROCESS_TESTS` and never reaped, so `process::reap` — `waitpid(-1)`,
  process-wide — collected a shell that had exited 0 and told the SIGKILL test it had a
  status. It failed on a twelve-core host and passed on a two-core one, which is the whole
  character of the bug: more tests in flight, more chances for somebody else's zombie to be
  the one reaped. Holding the lock is not enough on its own, because a *finished* child is
  still there after the lock is released.
- **`sfdisk` is translated, and the GPT tests read English out of it.** On a Polish host
  `sfdisk --list` answers `Typ etykiety dysku: gpt` and a test grepping for `Disklabel type`
  fails about bytes that are perfect. Any test that greps a util-linux program's output has
  to run it under `LC_ALL=C`.
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

1. **Name — decided.** The product is **MediaLith** (2026-08-11). This entry existed
   because "PlexOS" used a third-party trademark; it no longer does, and the disclaimer
   in the README states the absence of any affiliation with Plex Inc.
   **The internal namespace was deliberately not renamed with it**, and that is the part
   still open: `/var/lib/plexos`, `/etc/plexos/config.toml`, `plexos.slot`, the
   `plexos-<version>.efi` boot entries, the manifest's `product` field and every crate
   name still say `plexos`. Each is a contract with a disk or with a release already in
   the field — see "Names that did not change" in the README. Moving any of them is a
   migration in which a release must accept both spellings for long enough that no machine
   is left behind, and it gets cheaper the sooner it is done and more expensive the more
   machines exist.
2. **Secure Boot keys.** Enrol our own, or go through Microsoft's shim process.
3. **Licence.** Not chosen.
