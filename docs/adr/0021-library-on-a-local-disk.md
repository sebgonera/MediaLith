# ADR-0021: A media library on a disk in the machine

**Status:** Accepted
**Date:** 2026-08-14

## Context

Everything MediaLith knew about where a library lives assumed a NAS. `plexosd::shares`
mounts NFS or SMB, the first-boot wizard's media step said "add the network share your
films are on", and the Storage view's empty state said "add the NAS or server the library
lives on below". All of it was written on a machine whose library is on a NAS, and all of
it is a description of that machine rather than of the product.

It is not the common case. The way somebody who did not build this actually meets it is:
they write the image to a USB stick, plug it into a computer they already own, and boot
it. Their films are **on that computer** — on the Windows partition of its internal disk —
or on an external drive they plug into it. In both cases the library is a few centimetres
from the processor that wants to read it, on a disk MediaLith can see, spin up, enumerate
and name in the installer's own disk list.

And could not open. Three things stood in the way, and only the first is obvious.

**The kernel could not read NTFS at all.** `CONFIG_NTFS3_FS` was `is not set`. FAT, exFAT,
ext4, XFS and ISO9660 were built, because those are what a Debian package arrives on
(ADR-0010's removable-media path) — and NTFS, which is what every Windows disk is, was
not. `plexosd::media` even carried the sentence "If it is NTFS, this kernel cannot read
it" as advice.

**There was no concept of a local library.** `shares.rs` is a list of servers, keyed by
protocol, with a hostname and credentials. A folder on a partition is none of those
things.

**And there was nothing on the console to choose one with.** Even given a mount, the page
had no way to say *which folder* — and a person cannot be asked to type
`/run/plexos/disks/b1cf4f2e-…/Users/seb/Videos`.

## Decision

### The kernel builds `ntfs3`, read-only is the point, and it is a built-in

`CONFIG_NTFS3_FS=y` and `CONFIG_NTFS3_LZX_XPRESS=y`.

The decisive detail was read out of `fs/ntfs3/super.c` in the 6.19.14 tree rather than
recalled, because guessing it would have produced a feature that fails on most of the
machines it is for. A Windows shut down by **Fast Startup** — which is the default — or
hibernated leaves `VOLUME_FLAG_DIRTY` set and an unreplayed journal, and `ntfs3` refuses
both. **Both refusals are guarded by `!ro`** (lines 1367 and 1373). MediaLith mounts media
read-only unconditionally, for reasons that predate this ADR, so the common case simply
works and nobody has to be told to go and shut Windows down properly before their media
server will read their films.

That makes read-only load-bearing in a second, independent way. It was already the rule
because a library is read and an appliance that could delete somebody's films on a bug is
a worse appliance. On an internal Windows disk — always somebody's only copy — it stops
being a preference.

`=y` and not `=m` matters and is asserted twice. `NTFS3_FS` is a tristate and
`CONFIG_MODULES` is on since ADR-0015, so kconfig has a third answer available for it, and
a module in an image with no `modprobe` is a filesystem that is silently gone. A unit test
pins the request in `linux.fragment`; stage 7b of `post-image-test.sh` pins the outcome
against the `.config` the kernel was actually built with, which is the half a fragment
cannot promise.

`LZX_XPRESS` is not optional either: without it a file somebody compressed with Windows'
`compact` is present, listed, and unreadable — the worst of the three states.

### A library is a folder on a partition, identified by `PARTUUID`

`plexosd::disks` records `{ name, partuuid, subpath }` in `/var/lib/plexos/disks.json`.

**Not a device name.** `/dev/sdb1` is what the kernel called something during one boot,
and recording it would give a library that comes back pointing at a different drive the
first time somebody plugs two things in. That is the label-ambiguity defect this project
has already paid for once, in the place where it costs a partition write.

`PARTUUID` is unique across disks and the kernel publishes it in every partition's
`uevent`. It exists for MBR as well as GPT — `block/partitions/msdos.c:113` synthesises
`%08x-%02x` from the disk signature and the partition number — so a USB drive partitioned
by Windows is covered as well as a modern one.

A volume with no `PARTUUID` at all (a filesystem written straight to a whole disk, no
partition table) can be browsed and **cannot be remembered**, and is refused with that
reason and a remedy rather than recorded as something meaningless.

### Ownership is passed explicitly, because these filesystems have none

NTFS, exFAT and FAT carry no Unix owner, so the kernel invents one from the mounting
process: `opts->fs_uid = current_uid()` and `fs_fmask_inv = ~current_umask()`
(`fs/ntfs3/super.c:1804`). `plexosd` is root. Plex runs as uid 900.

Left implicit, that is the render-node defect exactly — every layer above reports success,
the mount is there, everything probing as root can list the files, and only Plex finds an
empty library. So `uid`, `gid`, `fmask` and `dmask` go in the option string and the result
does not depend on what umask the daemon happened to inherit.

ext4 and XFS are the other way round and **cannot** be fixed here: they carry real
ownership from the machine that wrote them. So the question is asked rather than assumed
— from the mode bits, not by attempting the read, because this process is root and root's
attempt always succeeds — and a folder Plex cannot walk into is reported at the moment
somebody chooses it.

### The chosen folder is bind-mounted under `/var/media`, before Plex starts

The volume is mounted under `/run/plexos/disks/<partuuid>`; the chosen folder is bound
from there to `/var/media/<name>`. Downstream, a local library and a network share are the
same thing: a directory under `/var/media` that existed before Plex was confined. That is
`shares.rs`'s doctrine unchanged, and it is followed rather than reasoned around for the
same reason it was adopted — whether a Landlock rule on `/var/media` reaches a filesystem
mounted underneath it afterwards is a question this project has already been caught
guessing at.

`mount(2)` with `MS_BIND` **discards every other flag** (`fs/namespace.c:4025` hands
straight to `do_loopback`) and the new mount copies the source's `mnt_flags` verbatim
(`fs/namespace.c:1256`). The bind is read-only because the volume under it is. `bind`
passes that flag alone, so that the next person to add `rw` there finds out from the code
that it would change nothing.

### Browsing is a `POST`, and the inventory is a `GET`

`GET /api/disks` lists drives, sizes, models and GPT partition names — no more than
`/api/install` already discloses, and it stays credential-free because a broken machine
has to remain diagnosable.

Looking *inside* a drive is `POST /api/disks` with `action: "browse"`, and so are `scan`,
`add`, `remove` and `release`. The folder names on somebody's Windows disk are the same
class of thing as the process list and what is playing: not a diagnostic, and not
something every reader on the LAN should have. The method-based gate in `http::refusal`
enforces it for all of them at once.

Every path the browser is given is canonicalised and required to be under
`/run/plexos/disks`, so `..` cannot walk out of a mount and turn this into a route that
reads any directory on the appliance to whoever names one.

### Plex can be restarted on its own

`POST /api/plex/restart` — `stop` then `ensure_started`, which is what `plex::swap`
already does either side of replacing an app image. Until it existed the only thing the
console could offer after adding a library was restarting the whole appliance, which stops
the console, the terminal and every other stream in the house to pick up a folder. The
shares card had the same gap and now shares the fix.

It is offered rather than done, and reported as "restarting" rather than "restarted":
Plex takes about twenty seconds to answer, and a page that claimed otherwise would be
lying about the thing it had just done.

### MediaLith's own partitions are refused, from the frozen layout

The disk the appliance is running from is excluded, resolved through dm-verity's `slaves`
the way the installer does. MediaLith's own partitions on *other* disks — the installer
stick, typically — are refused by matching against `plexos_types::partition::LAYOUT_X86_64`
rather than a list written here, so a seventh partition added to ADR-0003 is covered
without anybody remembering this file.

And when the running disk cannot be established, **every** volume is refused. "I do not
know" and "nothing is excluded" are the same value and opposite meanings.

## Alternatives considered

**Copy the library onto `/var`.** `/var` is what is left of the disk MediaLith is
installed on, sized for a database and app images. Nobody's film collection fits, and the
one thing that must not fill is the partition a rollback cannot repair.

**Mount the whole volume rather than a chosen folder.** Simpler, and it grants Plex the
whole of somebody's Windows installation — every document, every Downloads folder — to
scan and to index. A bind mount of the folder they actually chose costs one extra mount.

**Identify a volume by its filesystem UUID.** More natural than a partition GUID, and it
needs superblock parsing per filesystem: NTFS's 64-bit serial, exFAT's, ext4's real UUID,
each at a different offset. The kernel already publishes `PARTUUID` for every partition on
both partitioning schemes, for nothing.

**Build `btrfs` as well.** Increasingly the default on Linux desktops, and it would widen
what a Linux user can plug in. Deliberately not done: it is a large filesystem to carry in
a kernel that is charged for four times over — initrd, both UKIs, every bundle — and
neither case this ADR is about is a btrfs drive. Recorded here so it is a decision rather
than an omission.

**Write to the drive.** Never, and not configurable. The only reason to want it is to use
the appliance as something it is not, and the drive in question is routinely somebody's
only copy.

## Consequences

A person who owns one computer can now boot MediaLith from a stick and point it at the
films already on that computer, which is the shortest path from "I downloaded an image" to
"it plays my library" and did not exist before.

The console can browse a filesystem, which is a capability it did not have and which is
worth naming as a widening of what an administrator credential is worth. It is bounded to
the scan root, it lists directories rather than files, and it is behind the same gate as
the process list.

`/var/lib/plexos/disks.json` joins the state on `/var` that a rollback deliberately leaves
alone. ADR-0009 permits the addition: a release that has never heard of the file ignores
it, and a library configured under this release survives a rollback to one that cannot use
it.

The image gains `ntfs3`. That is a filesystem driver in the trusted computing base being
pointed at a disk MediaLith did not create and does not verify — which is equally true of
the exFAT and ext4 drivers that were already there for ADR-0010, and is why every one of
these mounts is `ro,nosuid,nodev,noexec`.

**This reads a library; it does not receive one.** Every mount here is read-only, there is
no SSH, no Samba and no NFS server in the image, and `curl` and `wget` are clients. So the
assumed route is that a drive is filled by the computer that owns it and then attached, and
"put some films on the appliance's own internal disk" has no answer. That is a deliberate
boundary of this ADR rather than an oversight, and it is the obvious next thing somebody
will want: an appliance with a blank 1 TB disk in it and no way to fill it is a fair
complaint. Whatever answers it needs its own decision, because a writable library is
exactly the rule this one is built on.

The image also cannot **create** an NTFS filesystem — `ntfs3` is a kernel driver and there
is no `ntfsprogs`. Reading a disk Windows wrote and making one are different capabilities,
and only the first is here.

## What has run

The kernel half is proven on the reference laptop: `0.1.0.202608141231` booted on slot b
with `CONFIG_NTFS3_FS=y`, and the console enumerated every partition on both attached
disks, correctly refusing the six belonging to MediaLith itself and offering the one that
did not.

**No NTFS volume has been opened yet.** The only non-MediaLith drive to hand was a blank
1 TB WD SN560 carrying an MBR partition with no filesystem in it, which refused all six
filesystems — correctly.

That first scan found a defect worth recording here because it is about this design rather
than about a typo. `mount_probe` produced a full account of what each filesystem said, and
**none of it reached the page**: the console asked again with a `GET`, and `report` derives
its answer from the machine as it is now, where a drive that would not mount is attached,
enumerable and unmounted — indistinguishable from one nobody has tried. `scanned` had the
same flaw, inferred from "is anything mounted". A report built purely from present state
cannot express an event. `LAST_SCAN` now keeps the outcome, and the page draws the scan's
own response rather than discarding it.

It also surfaced a limit of the identity chosen above: a disk that was never given an MBR
signature enumerates as `00000000-01`, and so would the next one. `PARTUUID` is unique
across disks only when something bothered to write a signature, and busybox's `fdisk`
cannot write a GPT that would settle it. Recorded rather than fixed: refusing the drive
outright would be worse than the collision it prevents, and the real remedy is a
partitioning path this appliance does not yet have.
