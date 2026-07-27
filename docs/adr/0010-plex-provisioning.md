# ADR-0010: Plex binary provisioning and redistribution

**Status:** Accepted
**Date:** 2026-07-27

## Context

Plex Media Server is proprietary software distributed by Plex Inc. under terms that do
not grant redistribution rights. PlexOS cannot ship it inside an image, mirror it, or
host a copy — regardless of how much more convenient that would be.

This is not a detail to resolve later. It determines what a PlexOS image *is*: not a
complete system, but a system that fetches its central component on first boot. That
shapes the installer, the first-boot flow, the offline story, and what "reproducible
build" can mean here.

It also means the project depends on a third party's distribution channel staying
available and stable, and on doing so in a way Plex Inc. would consider legitimate.

## Decision

**PlexOS images never contain Plex Media Server.** Images are freely redistributable
on their own terms.

**Provisioning happens at first boot**, in the setup wizard, from Plex's own official
distribution endpoints — the same artifacts a Debian user would install, fetched
directly from Plex, not proxied or cached by us:

1. The wizard states plainly that Plex Media Server is proprietary software downloaded
   from Plex Inc., subject to Plex's terms, and requires the user's acceptance before
   any download begins.
2. The official Debian package is downloaded, its GPG signature verified against
   Plex's published repository key, and its payload converted into a PlexOS app image
   (ADR-0007).
3. The resulting image's hash is recorded so that later mounts are integrity-checked
   even though the artifact was not built or signed by us.
4. Plex account sign-in and server claiming happen through Plex's normal flow. PlexOS
   never sees or stores account credentials.

**Offline installation is supported** by letting the user supply the official package
on removable media, verified the same way. A media server in a cupboard may well have
no outbound internet at setup time, and this must not be a dead end.

**Existing installations keep working if provisioning breaks.** If Plex changes its
packaging or endpoints, already-provisioned devices continue to run from their local
app images. The failure is confined to new installs and new Plex updates, and it is
reported as such rather than as a broken system.

## Alternatives considered

**Bundle Plex and ask forgiveness.** Rejected outright: it is not ours to
redistribute.

**Seek a redistribution agreement with Plex Inc.** The right long-term move if the
project gains users, and it would remove the whole first-boot dependency. Not
something to block on now — the design above works without an agreement, and does not
foreclose one.

**Mirror the official packages on our own infrastructure for reliability.** Rejected:
still redistribution, plus it puts us in the position of serving binaries we cannot
vouch for.

**Ship Jellyfin instead, which is open source and freely redistributable.** It would
eliminate this ADR entirely. Out of scope: the project's purpose is a good Plex
appliance, and pluggable backends would compromise the tight integration — the
transcode self-test, storage layout, and health model — that justifies building an OS
at all. The app image boundary (ADR-0007) leaves the option open without designing for
it now.

## Consequences

- A PlexOS image is not self-contained. First boot requires either network access to
  Plex's servers or a user-supplied package.
- Images are not reproducible end-to-end: the OS is, the deployed Plex version is
  whatever upstream served. Recording the provisioned artifact's hash is what makes a
  given device's state auditable after the fact.
- The project carries a hard dependency on Plex's Debian packaging format and
  repository signing key. Both need monitoring, and a packaging change upstream is a
  release-blocking event for new installs.
- Legal review is required before any public release, covering the name "PlexOS"
  itself — which uses a third-party trademark and may need to change — as well as the
  provisioning flow and the wording of the terms shown in the wizard.
- The first-boot wizard is on the critical path for v1. It cannot be deferred, because
  without it there is no Plex.
