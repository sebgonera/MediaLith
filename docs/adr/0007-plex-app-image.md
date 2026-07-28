# ADR-0007: Plex as an independently versioned app image

**Status:** Accepted
**Date:** 2026-07-27

## Context

Plex Media Server releases roughly weekly. The OS underneath it should not.

Coupling them means every Plex release becomes a full OS image build, a
several-hundred-megabyte download, and a reboot — for a change that touches one
userspace binary. Users who fall behind on Plex releases lose client compatibility, so
"just update less often" is not available.

There is also a licensing constraint that makes the coupling impossible rather than
merely unpleasant: Plex Media Server is proprietary and cannot be redistributed inside
a PlexOS image at all (ADR-0010).

## Decision

Plex ships as a read-only filesystem image file, stored under `/var` and mounted at
runtime:

```
/var/lib/plexos/apps/plex/
    1.41.3.9314.img        version-named, immutable
    1.41.4.9463.img
    current -> 1.41.4.9463.img
```

- Independently versioned; the OS does not encode a Plex version.
- Updating Plex is a download, an atomic symlink swap, and a service restart. No reboot.
- The previous image is retained, so a bad Plex release is reverted by moving one
  symlink — the same rollback shape as the OS, at a different granularity.
- Images live on `/var` rather than in dedicated partitions, so retention policy stays
  a runtime decision instead of being frozen into the GPT (ADR-0003).

Plex runs as an unprivileged user, confined with cgroup v2 limits, Landlock, and
seccomp, with access to exactly: its data directory, configured media paths
(read-only), the transcode directory, and the GPU render node.

## Alternatives considered

**Plex inside the `/usr` image.** Simplest to build and gives a single trust domain.
Rejected on both grounds above: weekly full-OS updates, and redistribution is not
permitted.

**Plex in an OCI container via the official image.** The mainstream answer, and it
works. Rejected because it drags a container runtime, image store, and networking
layer into a single-purpose appliance to run exactly one workload — and GPU device
passthrough into containers is a recurring source of exactly the transcoding failures
PlexOS exists to eliminate. A mounted image with a confined process achieves the same
isolation with far fewer moving parts.

**Plex unpacked into a plain directory on `/var`.** Simple. Rejected: no atomicity
(an interrupted unpack leaves a half-installed Plex), no integrity check after
install, and no clean rollback. A single image file gets all three.

## Consequences

- Two update mechanisms exist — OS and app — and both need their own UI, scheduling,
  and failure reporting. This is real added complexity, accepted knowingly.
- `/var` holds executable content. It is mounted `nosuid,nodev`, and app images are
  verified against a hash recorded at provisioning time before being mounted.
- The app image boundary is where a second media server backend would attach. That is
  explicitly out of scope for v1 — the target is Plex — but nothing in this design
  forecloses it.
- Retaining previous versions costs disk. Policy: keep the current image and one
  previous.
- Plex must be provisioned before the system is useful, which makes first-boot setup a
  required flow rather than an optional one (ADR-0010).

## Amendment, 2026-07-28: what Landlock must also grant

The confinement above was first written as four paths — the app image, Plex's data, the
transcode scratch, and the render node. On the appliance that produced a Plex which
started and was dead in the same instant, with the reason on a screen and not on the
network.

A Landlock ruleset that *handles* every filesystem operation denies everything it does
not grant, and that includes the dynamic loader opening libc. Measured rather than
argued: applying exactly those four rules and then running `/bin/echo` fails with
`EACCES`, and `/proc/self/status` is unreadable.

The policy therefore also grants, read-only:

- **`/usr`**, read and execute — the loader, libc, and every program Plex shells out to.
  It is a read-only dm-verity mount, so this is what every process on the machine
  already has and nothing Plex can change.
- **`/etc`** — `resolv.conf`, `localtime`, and `passwd`, without which `getpwuid(900)`
  fails and Plex cannot learn its own account.
- **`/proc`** — `self`, `cpuinfo`, `meminfo`.
- **`/dev`**, plus write — `/dev/null` and `/dev/urandom`.
- **`/sys`**, not required — hardware discovery, with software transcoding as the
  fallback.

What this deliberately still does not grant is the part that carries the security value,
and it is asserted by a test rather than left to inspection: **`/var` is not granted**.
Plex reaches `/var/lib/plex` and `/var/cache/plex-transcode` and nothing else under it —
in particular not `/var/lib/plexos`, which holds the device token. A Plex that could read
that token could install whatever it liked through the console.

The lesson is narrower than "Landlock is hard": a deny-by-default policy has to be
*executed* before it can be believed, because the paths a process needs are mostly ones
nobody thinks to list.
