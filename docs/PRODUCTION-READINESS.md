# Production readiness audit — 2026-08-06

What stands between the system in this repository and one that can be handed to a
person who did not build it. Compiled by reading the tree against the working notes,
running the full test suite on a clean host, and sweeping for the failure shapes this
project has already recorded: features that are complete, tested and uncalled; notices
that stopped being true; and lists that describe one machine.

A summary of what already demonstrably works is at the end, because the point of this
document is the gap, not the achievement. The short version of the verdict: **the
appliance works end to end on real hardware, and nothing about its trust chain,
distribution, or legal identity is production-shaped yet.** The distance is mostly
operational and procedural, not architectural — which is the good position to be in,
and also the kind of work that does not show up by itself.

---

## Revision — 2026-08-10

The audit was compiled on 2026-08-06 and reached `main` four days later. The body below
is kept **exactly as written**: a record of what was true on a date is worth more than one
quietly edited to stay flattering, and the sections that closed are evidence the list gets
worked rather than admired. What changed in those four days is here instead, each line
checked against the tree on the day of the merge rather than recalled.

**Closed since:**

- **§1.5, the licence half** — Apache-2.0, in `LICENSE`.
- **§2.3 CI never runs** — `main` exists and is the default branch, so `ci.yml`'s push
  trigger fires. The stale branches named there have been deleted; the repository now has
  one line of history and no orphans. What remains of that section is the last sentence:
  an image is still built only by hand, on a host that has the Buildroot tree.
- **§2.4, the `xe` half** — `install_xe_firmware` in `post-image.sh` puts the whole
  directory into the initrd under `xe/`, which is where the driver asks for it. The claim
  about Arc has moved from "absent" to "shipped and unverified"; no such card has been
  tried, and `post-image-test.sh` asserts the blobs rather than the hardware.
- **§5, the `README.md` half** — rewritten to describe the system that exists.
- **§2.1, two of three** — `remember_password` and `unmount_one` have callers now
  (`shares.rs:1063` and `shares.rs:1116`). `kernel_says` still has none.
- **§3.6, the NVIDIA half** — ADR-0015's spike happened on the RTX 5060 the section named:
  it decodes with NVDEC and encodes with NVENC, and the GPU report answers the NVIDIA
  question rather than the VA-API one.
- **§4, the first bullet** — both halves of ADR-0010's offline path exist. A browser can
  hand the appliance a package (`/api/provision/upload`, streamed to disk under a
  `MAX_UPLOAD` of 256 MiB, with a button on the page), and a package can be chosen off
  removable media (`/api/media`, `/api/provision/media`) — which is the half a browser
  cannot do. Both end in the same verified pipeline as the download.
- **§1.2, the decision half** — ADR-0017 chose own keys over shim, and `post-image.sh`
  signs the bootloader as well as both UKIs. The other half is untouched and is now the
  single most conspicuous gap in the trust chain: **no firmware has ever enrolled the key**,
  so no machine has yet booted with Secure Boot enforcing. `PK` is what switches
  enforcement on, and with only `db` enrolled the platform stays in Setup Mode while
  reporting otherwise.

- **§2.4, the Wi-Fi half** — the list is no longer the two parts in this laptop. Four
  `iwlwifi` families are carried, chosen by reading `linux-firmware.mk` rather than by
  their numbers, and the Realtek side gained the PCIe symbol after the RTX desktop's
  onboard NIC asked for `rtl_nic/rtl8168h-2.fw` and did not get it — a link that still came
  up at 1Gbps, without the vendor's errata fixes, silently. `wireless-regdb` is there too.
  What stays true is the shape of the trap rather than this instance of it: the list is
  still assembled from machines that have been in the room.

**Re-verified as still open on 2026-08-10:** all of §1.1, §1.3 and §1.4; the name in §1.5;
`kernel_says` in §2.1; the host-dependent terminal test in §2.2 — it passes on the build
host and in CI and still opens the real `SHELL` and asserts a duration, so the finding is
about what it measures, not about where it is red; and every part of §3.1 through §3.5.

One number in §6 is stale in the direction that matters least: 331 passing tests have
become 835.

## Revision — 2026-08-11

**§3.1, the "nothing checks free space" half — read, not enforced.** `plexos_sys::fs::space`
wraps `statvfs(3)`, and the console's activity card reports `/var` and `/` against a severity
threshold, so a partition at 92% is now visible from a browser instead of from a shell. The
rest of §3.1 stands and the sharper half of it is untouched: **nothing refuses on the
number.** There is still no check before staging an ~85 MB download and no `ENOSPC`-specific
remedy in the update or provisioning paths, so a full `/var` remains a failure rather than a
warning. Seeing a full disk and being unable to fill one are different problems, and only the
first is done. Nothing in §3.1 about `/var/cache/plex-transcode` never being pruned has
changed.

**§2.2's neighbourhood gained a second instance, worth recording where the first one is.**
Two tests in `plexosd` now fail on any development host that runs its own Plex —
`plex::tests::a_handle_that_has_started_nothing_reports_nothing_running` and
`an_unprovisioned_machine_is_told_where_plex_would_be_rather_than_failing`, both through
`Handle::is_running`, which probes `127.0.0.1:32400`. The probe is right; ADR-0005's gate has
to ask the port rather than only its own child. The suite is what is wrong: it is not
hermetic, so a clean `cargo test --workspace` is impossible on such a host and CI being green
says nothing about it. Same shape as the terminal test already in §2.2 — a test that
describes the machine it was written on.

---

## 1. Release blockers

Things that must be resolved before a unit ships to anybody who is not the author.
None of them is optional and none is architectural.

### 1.1 The root key is a development key, and swapping it is a project, not an edit

`plexos_update::trust::ROOT_KEYS` (`crates/plexos-update/src/trust.rs:83`) holds one
key, marked `development: true`, whose private half is an unencrypted file on the
build host. Every layer honestly reports this — the console page says "this update
chain ends in a development root key" — which is exactly right for now and exactly
what cannot ship.

Moving to a production root involves more than replacing the constant:

- **Key ceremony and custody.** `plexos-sign` writes private keys as bare base64
  seeds — no passphrase, no encrypted container (`plexos-sign.rs:21`). A production
  root needs offline generation and storage that a build-host compromise cannot
  reach; ADR-0006 already asks for this and nothing provides it.
- **The fleet problem.** Root keys ship inside the image, so a device only trusts
  roots that were compiled in when it was flashed. Introducing a new root means
  shipping a release that carries *both* keys, signed by the old one, and removing
  the old key only after the fleet has taken it. There is no in-band root
  revocation — the revocation list covers signing keys only
  (`trust.rs:425`) — so a compromised root means reflashing. This is inherent to
  the design and fine, but the sequence needs writing down before the first
  production key exists, because a mistake here strands devices.
- **Two pieces of code assume the dev key.** The test at `trust.rs:794` asserts that
  *every* compiled-in root is a development key, and will fail — correctly, loudly —
  the moment a real one is added. `root_id_hint()` in `plexos-sign.rs:177` hardcodes
  `plexos-root-dev` and its own comment says it must become an argument when rotation
  is real.

### 1.2 Secure Boot: the one undecided decision ADR-0004 says must precede a public image

The dm-verity half of verified boot is done and proven. The signing half is not
started: `post-image.sh` signs the UKI only if `PLEXOS_SB_KEY`/`PLEXOS_SB_CERT` are
set, they never are, and every image so far has required Secure Boot off in firmware.
ADR-0004 explicitly defers the key-handling decision — enrol our own keys versus
Microsoft's shim process — and says it "must be resolved before the first public
image". It is still unresolved, and it has a long lead time if the answer is shim.

Two consequences worth naming:

- Until the UKI is signed, the kernel command line is attacker-editable, which
  weakens statements elsewhere in the tree that lean on "the command line lives
  inside the signed UKI" — including the comment in `plexos-init/src/cmdline.rs:97`.
- `plexos.debug_shell` is parsed (`cmdline.rs:107`) and acted on by nothing but a
  `--dry-run` printout. ADR-0004 asks for a *separate build variant* for debug
  boots, never a runtime flag on a production image. Before this parameter acquires
  a consumer it should be deleted or moved behind a compile-time feature.

### 1.3 There is no production update channel

`tools/publish-update.sh` is `python3 -m http.server` over a directory, says so, and
is right to be that for development. The design already permits a trivial production
channel — the manifest is signed over exact bytes and sources are bare file names,
so any static HTTPS host or object store works with no re-signing — but nothing
provides one, and three pieces around it are missing:

- **Channels.** The manifest has a `channel` field; `sign-bundle.sh:97` hardcodes
  `"dev"`. Nothing selects a channel on the appliance.
- **Discovery.** Every update today is an operator POST with an explicit source URL.
  There is no configured default source (`DEFAULT_SOURCE` is deliberately empty),
  no periodic check, and no notification that an update exists. For an appliance
  whose owner is not its builder, "updates happen when someone types a URL" is not
  an update story.
- **Revocation publishing.** The appliance fetches `revocations.json` from the update
  source, and nothing in `tools/` publishes one — issuing a revocation is a manual
  `plexos-sign revoke` plus hand-copying. The one mechanism that exists for a
  compromised signing key needs to be as routine as publishing an update, because it
  will be needed at the worst possible moment.

### 1.4 The appliance cannot tell the time, and the trust chain consults the clock

There is no time synchronisation of any kind — `plexos-update/src/clock.rs` says so
in its header. The clock decides certificate expiry in the update trust chain, TLS
validation when fetching Plex from `downloads.plex.tv`, the validity of the
console's own certificate as a browser sees it, and every timestamp Plex writes.
The clock-plausibility check (an image cannot predate its own build stamp) bounds
the damage but does not correct anything. A machine with a dead RTC — the case the
console certificate's 1975–4096 validity was designed around — currently has a wrong
clock forever. An SNTP step at network bring-up, with the same "failure is not
fatal" posture as the rest of boot, closes this.

### 1.5 Name and licence

Two open decisions, and only one of them is still blocking:

- **The product name is settled: MediaLith** (2026-08-11), and it borrows nobody's
  trademark. What this entry warned about has happened in the cheapest possible form —
  the rename covered everything a person reads and **deliberately touched nothing on
  disk**, so no machine needed migrating and a rollback to a release published under the
  old name still works.
  What is left is the internal namespace: `/var/lib/plexos`, `/etc/plexos/config.toml`,
  the `plexos.` boot parameters, the `plexos-<version>.efi` entries and the manifest's
  `product` field. None of it is visible to an owner, and each is a contract with a disk
  or with an installed release, so moving it needs a release that accepts both spellings
  for long enough that no machine is left behind. Still cheapest before the first unit
  ships; no longer blocking, because nothing about it is wrong — only inconsistent.
- **No licence is chosen.** Nothing can be distributed at all until one is. The
  image also aggregates GPL components (kernel, Buildroot packages), so shipping
  binaries carries source-offer obligations that need an answer (Buildroot's
  `legal-info` machinery exists for exactly this and is unused).

---

## 2. Defects found by this audit

### 2.1 Share credentials can never be stored — the fourth "complete, tested, uncalled" feature

`plexosd::shares::remember_password` (`crates/plexosd/src/shares.rs:340`) writes a
NAS password to `/var/lib/plexos/share-credentials.json` with the right permissions,
is tested, and **has no caller**. Nothing on the `/api/shares` route accepts a
password, so `CREDENTIALS` is never written and an SMB share that requires
authentication cannot be mounted through the console at all. This is the same shape
as the `auth` gate, `cgroup::delegation` and the ADR-0008 configuration model — the
trap list's own grep-for-callers rule, confirmed a fourth time.

Two smaller instances in the same file: `unmount_one` (`shares.rs:573`) exists and
no route reaches it, so a share once mounted cannot be unmounted from the page; and
`kernel_says` (`shares.rs:612`) — written specifically so NFS refusals from
`/dev/kmsg` could be read over the network — is never invoked, so those messages
remain readable only on the attached screen, which is the condition it was written
to end.

### 2.2 A test's outcome depends on the host's shell, and one host has been found where it always fails

`terminal::tests::a_poll_waits_for_output_rather_than_returning_immediately_empty`
(`crates/plexosd/src/terminal.rs:485`) failed on every run in this audit's
environment (three of three), while 331 other tests passed — and then passed in CI
on ubuntu-latest the same day, which sharpens the diagnosis rather than dismissing
it. The test opens the real `SHELL` (`/bin/sh -l`) and asserts the second poll
blocks ≥250 ms; whether it does depends on when the host's shell emits prompt or
profile output relative to the first poll, which is a property of the host, not of
the code under test. A test that is green on two machines and reliably red on a
third is the trap list's fixture rule wearing a new coat: it measures the shell it
happens to run against. Separately, the known one-in-twenty-five flake
(`plexos-plex::a_mkfs_without_the_compressor`, suspected `ETXTBSY`) remains
unreproduced and unresolved; a suite that must gate releases cannot carry a known
flake indefinitely.

### 2.3 CI never runs on push, because it watches a branch that does not exist

`.github/workflows/ci.yml` triggers on `push: branches: [main]` and on pull
requests. The remote has no `main`: current work lives on `worktree-net-link-up-fix`,
with an older diverged `claude/linux-plex-media-distro-mowdbe` (9 commits not in the
current line) and `claude/image-assembly` beside it. So the push trigger has never
fired, and the repository's history has no canonical branch. Production needs: a
default branch that exists, CI that runs on it, the stale branches reconciled or
deleted — and, longer-term, an image build in CI, because today `post-image-test.sh`'s
47 checks run only on a build host that has the Buildroot tree, by hand.

### 2.4 Firmware lists that describe the machines already owned

- **`xe` has no firmware in the image at all** — `install_gpu_firmware` in
  `post-image.sh` globs `i915/` only. `CONFIG_DRM_XE=y`, so an Arc card binds and
  then runs degraded or not at all: the exact defect the GuC/HuC glob was widened to
  fix, one directory over. Any claim that current Arc works today is unverified and
  probably false.
- **Wi-Fi firmware covers two chips** — `BR2_PACKAGE_LINUX_FIRMWARE_IWLWIFI_9XXX`
  and `RTL_815X` are the reference laptop's parts. Every other radio will probe,
  find nothing, and produce a machine whose Wi-Fi silently does not exist. Same
  one-machine-list trap, third appearance.

---

## 3. Robustness gaps to close before production

### 3.1 `/var` grows in one place nothing manages, and nothing checks free space

App images are pruned (`KEEP = 2`), update staging is cleared, superseded boot
entries are removed — those lessons are encoded. But `/var/cache/plex-transcode`,
Plex's `TMPDIR`, is never cleaned by anything, and it is the largest and least
bounded writer on the partition. And there is no free-space check anywhere: no
`statvfs` before staging an ~85 MB download, no `ENOSPC`-specific remedy in the
update or provisioning paths. The ESP already demonstrated what an unwatched
partition does at 100% (the truncated `+3` UKI); `/var` is the same story waiting,
with Plex's database on it. The migration-backup cap ADR-0009 promises ("capped at
three, oldest pruned") is also not implemented — currently moot with one layout
version, but it is policy without code.

### 3.2 Nothing persists a log, so a crash loop erases its own evidence

`/var/log` is in the ADR-0009 layout and nothing has ever written to it. All
diagnostics — Plex's captured output, update progress, provisioning logs — live in
bounded in-memory rings that die with the daemon. `rollback.json` preserves the
gate's verdict, which is the single most important record and is handled; everything
around it is gone after a power cycle. For a machine in somebody's cupboard, the
question "what happened last Tuesday" currently has no answer. A small, bounded,
rotated on-disk ring for the supervisor and gate would close most of it.

### 3.3 The HTTP server spawns a thread per connection with no ceiling

`http.rs` accepts and spawns unboundedly (`listener.incoming()` →
`thread::spawn`). Head and body limits and the 15 s I/O timeout bound each request;
nothing bounds their count, and the long-polling terminal and update routes hold
threads for tens of seconds by design. Anyone on the LAN can exhaust memory without
a credential. The threat model is a trusted LAN, but "trusted" has so far meant
"does not attack", not "contains no misbehaving device". A simple counted semaphore
around accept would do.

### 3.4 The kernel tracks "latest" and nobody has decided the maintenance cadence

`BR2_LINUX_KERNEL_LATEST_VERSION=y`, with the defconfig's own TODO ("revisit whether
an LTS series is wanted before the first public image") still open. Two problems in
one: builds are not reproducible over time (the kernel changes under the same
defconfig), and there is no stated policy for shipping CVE fixes in the kernel,
OpenSSL, curl, or wpa_supplicant. An appliance that updates unattended needs a
declared cadence — "we rebuild and publish monthly and on severity" or similar — and
a pinned kernel series is the precondition for saying it honestly.

### 3.5 Two in-kernel file servers are compiled in and nothing manages them

`CONFIG_SMB_SERVER=y` (ksmbd) and `CONFIG_NFSD*=y` are built in for the planned
export feature; the management layer (`plexos-shares` in the old plan) does not
exist, and `plexosd::shares` is the *client* side (mounting a NAS). Dormant today —
nothing starts them — but they are the largest remote attack surface in the kernel
config, present in every image, covered by no ADR. Either the export feature gets
scheduled or these come out until it does.

### 3.6 Hardware coverage is honest but narrow

One confirmed GPU entry (`8086:3ea0`); the other four in the table are marked
"documented, unverified". Four machines ever booted, all Intel. NVIDIA is planned
in detail (ADR-0015) and unscheduled; AMD is off. That is a defensible v1 scope —
but the trap list's own summary ("a thing that is true about the machine it was
written on is not a thing that is true") says what to expect from machine five.
Production needs a supported-hardware statement users can read *before* buying,
and ideally the ADR-0015 steps 1–2 spike, since the RTX 5060 desktop exists to
verify it on.

---

## 4. Acknowledged feature gaps, restated so they are not lost

- **Upload from local disk / removable media** (ADR-0010): asked for, deferred; the
  streaming route `http.rs`'s `MAX_BODY` comment describes does not exist.
- **Wi-Fi**: joined a real WPA3 network on the appliance per the recent commits, but
  the module's own notice still claims association has never run — reconcile it, and
  record which security modes have actually been demonstrated.
- **Power off** (`RB_POWER_OFF`) has never been exercised on hardware; restart has.
- **Delta updates**: designed for in the manifest, not implemented — fine, full
  images are ~85 MB.
- **First-trust of the console fingerprint** still requires the attached screen once
  (ADR-0014, deliberately unresolved). Worth revisiting only when there is a
  companion channel to pin through; until then it stays a documented limit.

---

## 5. Documentation drift

In this project the prose is the spec, so a stale sentence is a defect, and the
audit found the notices badly behind the machine. The rule in the working notes —
"keep those notices accurate; delete them only when the thing has actually run" —
is currently violated in both directions.

- **`README.md` is the worst and the most public.** It still says updates are
  unsigned, there is no supervisor, no installer and no first-boot wizard; that the
  console is a bearer token over plain HTTP; and it lists `plexos-storage` and
  `plexos-shares` as if they were coming. Every one of those statements is now
  false, and the security warning it carries misdescribes the actual (better)
  posture. A newcomer reading the front page learns the state of June.
- **Stale "What has run: Nothing on hardware" notices** contradicted by the working
  notes and commit record, at least: `plexosd/src/tls.rs` (TLS proven on the wire
  2026-07-30), `install.rs` (the installer wrote the internal disk),
  `rollback.rs` (rollback has run twice), `terminal.rs`, `settings.rs`,
  `wifi.rs`, `plexos-update/src/trust.rs` and `sequence.rs` (a signed update was
  installed end to end), `plexos-sys/src/landlock.rs` and `pty.rs`,
  `plexos-init/src/supervise.rs`. Each should be deleted or rewritten to say what
  actually ran.
- **`plexos-sign.rs:33` says `ROOT_KEYS` "is still empty"**; it has held the dev key
  since trust landed.
- **ADR-0013 disagrees with the code twice**: it specifies a 256-bit token where
  the code ships 80 bits (well-reasoned in `auth.rs:60`, never amended in the ADR),
  and its Context still says "there is no TLS". ADR-0014 got a revision note for
  the TLS change; ADR-0013 needs the same treatment.
- **`docs/ARCHITECTURE.md`** still lists planned crates and the old component
  table.

---

## 6. What is demonstrably done

For balance, and because it is most of the system: A/B updates signed end to end
with anti-rollback and root-signed revocation, exercised on hardware including the
refusal cases; automatic rollback in both failure modes (unbootable image, booted
but unhealthy), each proven with nobody touching the machine; verified `/usr` under
dm-verity; an installer that wrote a real disk and the first-boot flow that follows
it; Plex provisioned from Plex's own signed packages, confined by Landlock and
cgroup v2 as uid 900, transcoding 4K HDR10 on the GPU; PID 1 that reaps, restarts
and survived having its services killed one by one; a TLS-only console with a
device token, terminal, network diagnostics, Wi-Fi, settings that report
stored-versus-applied; and 331 passing tests whose fixtures are captures rather
than guesses. The trap list exists because each of those was earned.

---

## 7. Suggested order

1. **Decide Secure Boot key handling** (§1.2) — longest lead time, blocks the image
   format conversation, and the shim path has external dependencies.
2. **Production root key ceremony and the rotation runbook** (§1.1), reworking the
   two dev-key assumptions in code.
3. **Name and licence** (§1.5) — the name feeds the key ceremony (certificates and
   `/var` paths carry it) and the licence gates any distribution at all.
4. **Repository hygiene: a real default branch, CI that fires, stale branches
   reconciled** (§2.3) — cheap, and everything after it benefits.
5. **The shares dead code** (§2.1) and the two firmware lists (§2.4) — small,
   already-diagnosed defects.
6. **Time sync, `/var` space management, persistent logs, connection ceiling**
   (§1.4, §3.1–3.3) — the operational hardening batch.
7. **Kernel series decision and CVE cadence** (§3.4), then the update channel and
   discovery (§1.3), which depend on knowing what will be published and how often.
8. **Documentation reconciliation** (§5) — continuously, starting with README.md.
