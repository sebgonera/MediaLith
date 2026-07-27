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
