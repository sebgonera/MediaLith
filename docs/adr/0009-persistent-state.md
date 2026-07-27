# ADR-0009: Persistent state layout and migrations

**Status:** Accepted
**Date:** 2026-07-27

## Context

`/var` is the only thing on a PlexOS device that cannot be recreated. The `/usr` image
is downloadable, the ESP is rebuildable, but a Plex library represents years of
metadata, watch history, and user accounts.

It is also the one part of the system that **rollback does not revert** (ADR-0005).
A new release that migrates `/var` into a shape the previous release cannot read has
built a trap: the OS rolls back correctly, and then fails on state it no longer
understands. This is the specific failure mode this ADR exists to prevent.

## Decision

### Layout

```
/var/
  lib/
    plexos/
      STATE_VERSION              single integer, the layout version
      etc/                       upper layer of the /etc overlay
      apps/plex/                 Plex app images (ADR-0007)
      update/                    download staging, accepted sequence, revocation list
      backup/                    pre-migration snapshots of config and small state
    plex/                        Plex Media Server data directory
  cache/
    plex-transcode/              scratch; safe to delete at any time
  log/
  media/                         default mount point for library storage
```

`/var/lib/plex` is Plex's application support directory, set via
`PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR`. Its internal structure belongs to Plex and
is opaque to us — we back it up and we never edit it.

### Migrations

`STATE_VERSION` is compared against the running release's expected version at boot,
by `plexos-init`, before any service starts.

- **Equal:** proceed.
- **State older:** run migrations in sequence, each atomic, each writing a backup into
  `/var/lib/plexos/backup/` first. Only then update `STATE_VERSION`.
- **State newer:** this is a rollback into an older release. Do **not** fail. Restore
  the pre-migration backup for anything the older release cannot read, log loudly, and
  boot. A device that will not boot after a correct rollback is worse than one running
  a slightly older configuration.

### The compatibility rule

**A migration may only add.** Renaming or deleting persistent state is split across
two releases:

1. Release *N* writes the new form, keeps the old form, and reads either.
2. Release *N+1* — shipped only once *N* is established as a known-good rollback
   target — stops writing the old form.

This makes every single release safely rollback-compatible with its predecessor, which
is the only compatibility guarantee the A/B scheme can actually offer.

### Backup boundary

Two categories, treated differently:

- **Small and ours** — config, app image metadata, update state. Snapshotted before
  every migration, kept for the last three migrations. Cheap.
- **Large and Plex's** — the library database and metadata. Not snapshotted at
  migration time. Exported on a schedule via Plex's own backup mechanism, because
  copying tens of gigabytes during boot is not viable.

## Alternatives considered

**btrfs snapshots of `/var` before each update, rolled back with the OS.** Elegant,
and it makes the whole problem disappear. Rejected for v1: it forces btrfs for `/var`
against XFS's better behaviour with large media files (ADR-0003), and rolling back a
Plex library would silently discard watch history and newly added media — data loss
the user never asked for. Worth revisiting for `/var/lib/plexos` alone on a separate
subvolume.

**Refuse to boot when `STATE_VERSION` is newer than expected.** The safe-looking
option, and wrong: it turns every rollback after a migration into a brick. The
rollback path must always terminate in a booted system.

**No `STATE_VERSION`, inferring layout from what is present.** Rejected: inference is
guessing, and guessing about the only irreplaceable data on the device is not
acceptable.

## Consequences

- Deleting persistent state always takes two releases. Slower, and it makes each
  release individually safe.
- Every migration needs a test that runs it forward, simulates a rollback, and asserts
  the previous release still boots. This test is as important as the migration.
- `/var` is mounted `nosuid,nodev`. App images under it are verified before mounting.
- `/var/lib/plexos/backup/` grows. Capped at three migrations, with the oldest pruned.
- Recovery media must be able to mount `/var` and extract a Plex library independently
  of any PlexOS release, since the worst-case recovery path is "the OS is beyond
  repair, get the library off the disk".
