# ADR-0020: Update channels, discovery, and a static update service

**Status:** Accepted
**Date:** 2026-08-12

## Context

ADR-0001 makes `/usr` the unit of update, ADR-0005 makes a new slot prove itself over three
boots, and ADR-0006 makes a release something only the holder of a key can publish. All
three work, and all three have been demonstrated on hardware — including the two failures
that matter, a bad image rolling back and a validly signed downgrade being refused.

What they do not add up to is an update *story*. Every update this project has ever
performed was somebody typing an address into a field:

```
POST /api/update {"install": true, "source": "http://192.168.2.165:8080/medialith-update"}
```

That requires the owner of the appliance to be the person who built it. It is the right
mechanism for a bench and it is not a product. An owner should not have to know what a
bundle is, what the build host's address is, what a slot is, or how a manifest is signed —
and the appliance should not have to be told that a release exists.

Three things were missing, and they are separable:

**Nothing selected a channel.** The manifest has carried a `channel` field since v1 and
`sign-bundle.sh` wrote the literal string `dev` into every one. `plexos_types::config` has
carried `[updates].channel` for as long, defaulting to `stable`, and **nothing read it**.
So an appliance configured for stable releases would install a development build without
comment. That is the fourth instance of the shape this project keeps recording: a complete,
tested, uncalled design.

**Nothing discovered anything.** `DEFAULT_SOURCE` is deliberately empty, there was no
periodic check, and no way for a machine to say "there is a newer MediaLith".

**Nothing published a revocation list as part of publishing.** Issuing one meant running
`plexos-sign revoke` and copying a file by hand.

## Decision

### Discovery tells the appliance *what exists*. The updater decides *whether it may be installed*.

These are separate responsibilities and they stay in separate code. `plexosd::discover`
resolves an address; everything after that address is the path that already existed —
`update::evaluate` runs the signature chain, the certificate, the revocation list, the
anti-rollback counter, the product check, the channel check and the slot arithmetic, and
`update::write_slot` writes the inactive slot exactly as before. **There is no second
updater and no shorter path through the checks.**

The consequence worth stating plainly: the channel file below is unsigned, and that is
safe *because* of the division. Whoever answers at the configured address chooses which
**signed** manifest this appliance evaluates. They can withhold an update, offer an older
release the counter refuses, or offer another channel's release the channel check refuses.
They cannot make the machine run their code.

### An update service is files on a web server, and nothing else

```
<base>/channels/stable.json          {"release": "…", "manifest": "releases/…/manifest-stable.json"}
<base>/channels/beta.json
<base>/channels/dev.json
<base>/releases/<release>/manifest-stable.json      and .sig
<base>/releases/<release>/manifest-dev.json         and .sig
<base>/releases/<release>/usr.erofs  usr.hash  plexos-<release>-a.efi  plexos-<release>-b.efi
<base>/releases/<release>/revocations.json          if one has been published
```

No database, no application, no authentication, nothing dynamic. `rsync` it to a web server
or upload the directory to object storage. The authenticity is in the signature over the
manifest; the transport is not trusted and does not need to be.

**Why the manifest lives in the release directory rather than the channel directory.**
ADR-0006 addresses artefacts by *bare file names*, resolved against wherever the manifest
itself was fetched from, so that a bundle can be moved between hosts without re-signing it
with a key that is meant to be offline. That rule decides this layout: the manifest has to
sit beside the artefacts it names. A channel therefore contributes a small pointer file and
a manifest *name*, not a directory of its own — which is also what keeps the artefacts
stored exactly once per release instead of once per channel.

### One feed per channel. No inheritance.

A stable appliance reads the stable feed and accepts a manifest whose `channel` is
`stable`. That is the whole rule. A beta device does not quietly take stable releases and a
stable device never sees a beta one.

Inheritance was considered and rejected. "Beta accepts beta or stable" sounds harmless and
makes "which of these two releases will this machine take" stop having a single answer,
which is precisely the question an owner is entitled to have answered.

An appliance configured to a channel this release cannot name — a fourth channel from a
future publisher — **checks nothing and says so**. It does not fall back to stable, because
falling back would take releases its owner did not ask for.

### Promotion re-signs metadata and never rebuilds an artefact

A release tested as `dev` becomes `stable` by writing a second small manifest naming the
**same digests**, signing it, and repointing a channel file. `tools/promote-release.sh`
digests every artefact on disk first and refuses if any of them is not the file the release
was signed with. Nothing is rebuilt and nothing is copied.

`channel` is inside the signed document, so a channel-specific manifest must be re-signed —
a four kilobyte document. The alternative, a channel that lives outside the signature, would
let whoever serves the files decide which machines take a release.

### A release identifier names bytes

Once `0.1.1.202608250900` has been published, no different artefact may ever be published
under that name. `tools/publish-release.sh` enforces it in both directions: the bundle must
match its own manifest, and a release already in the tree must present identical digests.
An appliance reporting "running 0.1.1.202608250900" has to be saying which operating system
it is running.

### Checking is automatic. Installing is not.

| | this phase |
| --- | --- |
| check automatically | **yes**, about every 24 h |
| download automatically | no |
| install automatically | no |
| restart automatically | no |

The first check waits five minutes after the daemon starts, so ADR-0005's gate has already
decided about this boot. Each appliance adds an offset under an hour, derived from its own
stored credential fingerprint rather than drawn at random, so that a fleet spreads out and
*stays* spread across the reboots a release causes.

**Update discovery is never part of the health gate.** An appliance whose update service is
unreachable is a healthy appliance. Nothing about DNS, TLS, the clock or a web server may
influence whether a slot becomes permanent — that separation is ADR-0005's and this ADR does
not touch it.

`[updates].automatic` already exists in the configuration schema and means "OS updates
install without being asked". **Nothing implements it and this release installs nothing
unasked.** It is left exactly as written rather than quietly redefined; automatic
installation is a later decision, after discovery has been shown to work on real machines.

### "Available" means verified

The console says *update available* only after the manifest has been fetched, its signature
verified against a certificate chaining to a compiled-in root key, the signing key checked
against the revocation list, the product matched, the channel matched, and the anti-rollback
counter satisfied. A check that fails says **update check failed** and why.

A cryptographic failure is never rendered as "no update available". Those are opposite
states, and reporting the first as the second is how an appliance under attack looks exactly
like an appliance with nothing to do.

### Configuration lives where a rollback cannot take it, in a section an old release ignores

The channel reuses `[updates].channel`, which every deployed release already has. The
address and the automatic-check switch go in a **new top-level section**:

```toml
[update_service]
url = "https://updates.example/medialith"   # empty: this appliance never looks
check = true
```

Not in `[updates]`, and the reason is rollback rather than taste. `Updates` is
`deny_unknown_fields`, so a key added there makes the whole configuration unreadable to
every release already in the field — and the release you roll back *to* is by definition an
older one. `Config` ignores unknown top-level sections (a property added 2026-07-29, before
any release now on a machine), so a section is tolerated where a key is not. A test
constructs the older release's parser and reads a file this one writes.

One honest limitation: if an older release *rewrites* the file after a rollback — somebody
changes the hostname — it serialises the structure it knows and the section is dropped. The
setting survives a rollback; it does not survive a rollback plus an edit made from the older
release.

### There is no MediaLith update service, and no address is baked into the image

The product default is empty and the shipped state is "system updates are not configured".
Inventing a domain that does not exist, or baking a developer's build host into every image,
would both be worse than saying so. The console says it plainly and offers the field.

## Consequences

**The bench workflow is unchanged and stays supported.** `tools/publish-update.sh` still
serves a bundle over HTTP and the console still takes a pasted address — now under a
disclosure labelled as what it is. That path is what an appliance with no route to a service
has, and it is what the appliance whose USB stick went read-only needed.

**Every publish now has to name a channel.** `sign-bundle.sh` has no default and refuses
without `--channel`. Defaulting to `stable` would put a development build on every machine
that forgot the flag; defaulting to `dev` would quietly do nothing on a stable fleet.

**A machine already in the field will refuse the next dev bundle until its channel is set.**
Every appliance defaults to `stable` and every bundle this project has ever signed says
`dev`. That is the correct behaviour arriving late rather than a regression, and the remedy
is one field in the console.

**Production HTTPS still depends on the clock.** This appliance has no time synchronisation
(`PRODUCTION-READINESS.md` §1.4). Certificate expiry in the update chain is already guarded
by a plausibility check — an image cannot predate its own build stamp — but TLS validation
against a real update service is not, and TLS validation is not disabled to make it work.
Automatic discovery works; **production HTTPS deployment remains blocked on the
clock-synchronisation work**, and this ADR does not close that.

**The root key is still a development key.** Its private half is on a build host. Every
place that reports a signature says so, and this changes none of that.

## Alternatives considered

**A dynamic update server.** Rejected: it would put availability of updates behind an
application nobody wants to operate, and it buys nothing — the trust is in the signature, so
a server that could choose what to serve can already do that with static files.

**Absolute URLs in promoted manifests**, so a channel directory could hold its own manifest
beside nothing. Rejected: it ties a signed document to one host, which is the thing
ADR-0006's bare names exist to avoid, and this project has served the same bundle from four
different addresses.

**Duplicating artefacts per channel**, so `manifest.json` could keep one fixed name.
Rejected: three copies of ~400 MB per release, and "the same bytes" would become a claim
about a copy rather than a fact about a file.

**Channel policy outside the manifest**, decided by which feed answered. Rejected: it makes
the channel a property of the transport, so whoever serves the files decides which machines
take a release — and that is exactly the authority the signature exists to withhold from
them.
