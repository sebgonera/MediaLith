# ADR-0005: systemd-boot with boot counting for rollback

**Status:** Accepted
**Date:** 2026-07-27

## Context

A bad update must undo itself with nobody present. That requires state that survives
a reboot, is written before the risky boot and evaluated after it, and is durable
against power loss at any instant — including during its own update.

It also requires a definition of success. "The kernel started" is far too weak: a
kernel that boots into a system where Plex cannot run has not succeeded, and it will
happily keep not-succeeding forever.

## Decision

**Bootloader: `systemd-boot`**, used as a standalone EFI application. It does not
imply systemd as PID 1 — PlexOS runs `plexos-init`.

**Rollback state: `systemd-boot`'s boot-counting convention**, which encodes the
counter in the UKI's filename on the ESP:

```
EFI/Linux/plexos-0.2.0+3.efi      3 tries left, none used
EFI/Linux/plexos-0.2.0+2-1.efi    after one failed boot
EFI/Linux/plexos-0.2.0+0-3.efi    exhausted; skipped in favour of the other entry
EFI/Linux/plexos-0.2.0.efi        marked good; no counter
```

The bootloader decrements by renaming the file before handing off to the kernel. FAT
directory entry renames are the closest thing to an atomic operation available this
early in boot, and there is no dependence on EFI variable writes, which have limited
write endurance and inconsistent firmware behaviour.

**Success is declared by `plexosd`, not by `plexos-init`.** The current entry is
renamed to its unsuffixed form only after:

- `plexosd` is responding on its socket,
- `/var` is mounted read-write,
- Plex Media Server is answering HTTP.

Until then the counter stands. Three bad boots and the previous slot wins.

## Alternatives considered

**Writing our own EFI bootloader in Rust.** Tempting for a project with "written in
Rust" as a goal, and technically achievable. Rejected: a bootloader is the one
component where a bug means an unrecoverable brick, `systemd-boot` already implements
exactly the counting scheme needed, and the effort buys no Plex-specific value. This
is the wrong place to spend novelty.

**GRUB with a persistent environment block.** Widely deployed and well understood.
Rejected: substantially larger, its scripting layer is a liability rather than an
asset for a fixed two-entry appliance, and `grubenv` writes are less obviously atomic
than a FAT rename.

**EFI variables for the boot counter.** The obvious place to put boot state. Rejected:
limited write endurance, and a genuine history of firmware bugs bricking machines on
variable writes. Not somewhere to write on every boot.

**`plexos-init` declaring success once services are spawned.** Rejected, and this is
the important rejection. Spawning is not working. An update that breaks Plex while
leaving PID 1 healthy would be marked good and never rolled back — precisely the
failure this mechanism exists to catch.

## Amendment, 2026-07-29: what actually spends a try

The decision above describes the counter and who clears it, and says nothing about what
*decrements* it. That turned out to be the whole of the mechanism, and neither half of it
existed.

A try is spent by booting. So a boot that fails has to end in another boot, and neither
failure shape did.

**An image that cannot boot.** `plexos-init` reports the fault, holds it on screen and
exits; PID 1 exiting is a kernel panic, and `panic_timeout` defaults to 0, which means
loop forever. The machine sat at a panic screen with three unused tries. `panic=20` on
the kernel command line is the fix, and it is asserted by `post-image-test.sh`, because
an absent parameter and a wrong one look identical from outside.

**An image that boots into a system that does not work.** The gate left the counter
standing, correctly, and then nothing restarted — so nothing consumed it. This is the
failure this ADR calls out as the important one, and it was the one with no path at all.
`plexosd` now restarts on an unhealthy verdict.

**But only when the booted entry is still being counted**, which is the substance of the
amendment. The counter exists to undo an *update*, not to repair a machine, and the three
other states each take a different answer:

| State of the entry | On an unhealthy boot |
| --- | --- |
| On trial, tries left | Restart. Three of these and the other slot takes over. |
| Already permanent | Stay up. There is no counter to spend, so restarting is an unbounded loop that removes the only means of diagnosis. |
| On trial, no tries left | Stay up. The bootloader gave up on this entry and booted it anyway, so there is nothing else to reach — the two-bad-updates case below, which needs recovery media and therefore needs a console. |
| ESP unreadable | Stay up. Asymmetric on purpose: a machine left running with a broken Plex can be looked at over the network, and one in a reboot loop cannot. |

**A rollback leaves a record on `/var`.** Reverting `/usr` destroys every explanation the
failed boot produced, and the system that comes back is the older one, which has no way to
know it is a replacement. `/var` survives precisely because of the rule two entries down —
rollback never reverts it — so the note is written there immediately before the restart
and served from `/api/update`. It covers the boots-but-unwell path only: an image that
cannot boot at all never reaches userspace, so nothing is there to write it down.

**An exhausted entry is not an entry on trial**, and conflating the two very nearly cost
the mechanism. `plexos-<version>+0-3.efi` still carries a counter in its name, so the
wreckage of a failed update satisfied every "is this on trial" test. Two consequences:
the gate reported an impending rollback on machines where nothing was going to roll back,
and — worse — the next genuine update would have found *two* entries on trial, been unable
to say which had booted, and quietly stopped restarting on an unhealthy boot. Rollback
would have worked once per machine and then disabled itself.

**Wreckage is removed by the next update.** Nothing removed it before, and each one is an
18 MB UKI on an ESP that ADR-0003 sized for three. It is the safest entry on the disk to
delete — `systemd-boot` will not choose it while any other exists — except when it is the
one that booted, which happens after two bad updates and is the case that must survive.

### This has now run

On 2026-07-29 the reference laptop was updated to a bundle whose `/usr` had its first
4 KiB block overwritten, with the hash tree and root hash left intact. Every check in the
update path passed, because every check in the update path asks whether the bytes offered
were the bytes stored, and they were. The machine restarted at 13:26:40, went unreachable
at 13:27:09, and answered again at 13:33:33 running the *previous* version from the
*previous* slot with an uptime of 22 seconds.

The evidence is the bootloader's own bookkeeping, left on the ESP:
`plexos-0.1.0.202607291323+0-3.efi` — three tries offered, three used, none left. Six
minutes and twenty-four seconds for three failed boots and one good one.

Verified boot and rollback were exercised together, which is the point of breaking the
image that way rather than the manifest: dm-verity loaded cleanly against an intact tree
and refused the first read of a block that did not match it.

## Consequences

- `systemd-boot` is a C dependency on the boot path, tracked and updated with the
  same care as the kernel.
- The health gate becomes safety-critical. If it is too strict, healthy systems roll
  back into an older image on a transient failure — an infuriating and hard-to-debug
  outcome. Its conditions must stay narrow, and each must be independently testable.
- Rollback protects against one bad update, not two. A device that has taken two
  consecutive bad updates has no known-good slot left and needs recovery media.
- Rollback returns the system to the previous `/usr` and kernel. It does **not** roll
  back `/var`. Any migration that rewrites persistent state must therefore stay
  readable by the previous release — see ADR-0009.
- The ESP must have room for three UKIs during an update. Sized for in ADR-0003.
