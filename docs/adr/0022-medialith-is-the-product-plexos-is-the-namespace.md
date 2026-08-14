# ADR-0022: MediaLith is the product; `plexos` is a frozen internal namespace

**Status:** Accepted
**Date:** 2026-08-14

## Context

The product was renamed from PlexOS to MediaLith on 2026-08-11, because the old name used
a third-party trademark. The rename covered everything a person sees: the README, the
attached screen, the console page, the boot messages, the release notes. A test in
`plexosd::console` asserts that the served page never contains the string `PlexOS`, and it
has held since.

What the rename deliberately did **not** cover is the internal namespace. A grep still
finds `plexos` around 1,900 times across 175 files: `/var/lib/plexos/**`,
`/etc/plexos/config.toml`, `plexos.slot` and `plexos.roothash` on the kernel command line,
`plexos-<version>.efi` boot entries, `product = "plexos"` in the update manifest, `ID` and
`SORT_KEY` in `os-release`, every crate and Buildroot package name, and 257 `PLEXOS_*`
build variables.

That was recorded in `CLAUDE.md` as **open decision #1** — "the internal namespace was
deliberately not renamed with it, and that is the part still open" — and in the README as
a table of names with the reason each must stay. Two documents, one of them calling the
question open and the other answering it.

An open decision costs something even when nobody acts on it. It invites the question
again at every audit, it makes a raw grep count look like an unfinished job rather than a
design, and — worst — it leaves the door ajar for somebody to start the migration halfway
through an unrelated change. This ADR closes it.

## Decision

**MediaLith is the product name. `plexos` is the internal compatibility namespace, and it
is frozen.**

Nothing in the following list changes, now or as part of any future tidy-up. Each is a
contract with a disk or with a release already in the field, and this appliance is built
so that a new release can fail and hand the machine back to an older one — which means
both spellings would have to be understood by every release in the overlap.

| Frozen | What breaks if it moves |
| --- | --- |
| `product` in the update manifest | The updater refuses a bundle whose product differs. A build claiming `medialith` is refused by every machine already installed |
| `/var/lib/plexos/**` | The one surface a rollback does not revert. The device token, the TLS key, the anti-rollback floor and the revocation list live here |
| `/etc/plexos/config.toml` | Renaming it silently reverts a machine's hostname, timezone and addressing to defaults |
| `plexos.slot`, `plexos.roothash` | Inside each signed UKI, including the previous release's. A build understanding only new names could not boot the image it falls back to |
| `plexos-<version>.efi` boot entries | Written by the release installing an update, read by the release booting it. A disagreement leaves the try counter uncleared and the machine rolls back three reboots later |
| `ID` and `SORT_KEY` in `os-release` | `SORT_KEY` is what systemd-boot groups entries by; a mixed ESP would be two groups |
| Crate, binary and Buildroot package names | A large diff, no user-visible benefit, and every build script and image-assembly step is a chance to break something that works |

`PLEXOS_*` build variables are not on that list because they are not contracts — they
never reach a disk. They stay anyway, for the same reason: renaming them is churn with no
reader who benefits.

**What must stay true, and is enforced:** nothing a person sees says PlexOS.
`plexosd::console::tests::the_page_calls_the_product_by_its_name` is the guard for the
console, which is the surface that matters most — and it checks the whole file, comments
included, because a comment still calling this PlexOS tells the next reader something
untrue about the product. The four remaining occurrences of the exact spelling
`PlexOS` outside the ADR set are all deliberate — a README note about the former working
name, a `CLAUDE.md` line explaining the rename, that test's own assertion, and a comment in
`esp.rs` stating the fact that a bundle installed by a machine still running PlexOS gets a
`plexos-` entry name.

**The ADRs keep their original wording.** ADR-0001 through ADR-0018 were written while the
product was called PlexOS and are dated records of decisions taken then. Rewriting them
would make them describe a world that did not exist at the time, which is the one thing a
decision record must not do.

## Alternatives considered

**Migrate the namespace now, while there is one machine in the field.** Genuinely the
cheapest this will ever be, and it was the argument for leaving the question open. It was
still declined: the cost is not the diff, it is that every path in the table above is
load-bearing during *rollback*, so a migration means a release that accepts both spellings,
kept in the field long enough that no machine can land on a release that understands only
one. That is a multi-release programme with a real chance of bricking the machine it is
performed on, bought for a cosmetic property nobody using the appliance can observe.

**Migrate only the cheap half** — crate names, package names, `PLEXOS_*`. This is the worst
option and worth naming so it is not proposed again. It produces a tree where `plexos`
means "on-disk contract" in some places and "we did not get round to it" in others, and
nobody reading it later can tell which is which without checking each one. The value of the
current state is precisely that `plexos` means exactly one thing: *frozen*.

**Rename the product back.** Not available; the old name used somebody else's trademark.

## Consequences

`CLAUDE.md`'s open-decisions list loses its first entry, and the README's "Names that did
not change" becomes the statement of an accepted decision rather than a note about an
unfinished one.

A future maintainer who greps and finds 1,900 occurrences has one place to be sent, and the
answer is not "not yet" but "no, and here is why".

If the namespace is ever migrated after all, it starts from here: this ADR is superseded by
one that sets out the dual-spelling overlap, how many releases it lasts, and how a machine
that misses the overlap is recovered. Nothing short of that is a migration; it is a way of
breaking rollback.
