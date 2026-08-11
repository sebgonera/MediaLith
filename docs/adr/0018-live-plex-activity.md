# ADR-0018: Live Plex activity, and the credential that makes it possible

**Status:** Accepted
**Date:** 2026-08-11

## Context

The console reports what the machine *is* — which slot booted, whether `/usr` verified,
how full `/var` is, whether the GPU can transcode. It has never reported what the machine
is *doing for anybody*. For an appliance whose entire purpose is to play films to people
in a house, that is the missing half: "is anything playing, for whom, and is this machine
doing it the cheap way or the expensive one" is the question an owner actually has, and
until now the only place to answer it was Plex's own dashboard.

The Plex view said so out loud. It carried a paragraph explaining that active streams
were absent because reading them needs a *Plex account* token rather than ADR-0013's
device token, and that holding such a credential was "a decision worth taking
deliberately rather than as a side effect of wanting a nicer page". This ADR is that
decision.

Three things had to be settled before any code, and two of them are about disclosure
rather than mechanism.

**Whether PlexOS may hold a Plex account credential.** It already does, and always has:
`Preferences.xml` on `/var` carries `PlexOnlineToken`, written by Plex itself when the
server was claimed. `plexos-plex` preserves that file across restarts precisely because
it is the one secret on the appliance that cannot be recomputed. The question is not
whether the credential is here — it is whether `plexosd` may *read* it.

**Who may see what is playing.** Every `GET` on this console answers without a
credential, deliberately: a console that will not say why a boot failed until you
authenticate has defeated the reason it exists. That argument is about diagnostics. A
film title, a username, a device name and a position in a film are not diagnostics.

**What happens when Plex is unwell.** The console must keep working. Nothing that reports
on Plex may become something the console depends on.

## Decision

### `plexosd` reads the account token, and it never leaves the appliance

`plexosd` reads `PlexOnlineToken` out of `Preferences.xml` per request, sends it to
`127.0.0.1:32400` in an `X-Plex-Token` header, and drops it. It is not cached, not
logged, not written anywhere, and not put in a query string — Plex accepts it as one, and
that would place a credential in every log and proxy on the path. There is no proxy on
loopback today, which is exactly the kind of assumption that stops being true quietly.

The browser talks only to `plexosd`; `plexosd` talks to Plex.

Plex's own answer is **not** proxied. It carries the library path on disk, the owner's
avatar URL, their public IP address and a `guid` identifying the account's copy of the
item. What leaves the appliance is a PlexOS-owned document with no field a credential
could occupy, and a test serialises a report built from a response containing a token and
greps the output for it.

### Live activity is a `POST`, and the method is the access control

`POST /api/plex/sessions`, so `http::refusal` — the one method-based gate in front of the
whole route table — requires the device token. This is the same mechanism as
`POST /api/metrics/processes` and the terminal, and for a stronger version of the same
reason. ADR-0014 established that "read-only" and "safe to expose" are different
properties; a process list with command lines was the first case, and a household's
viewing history is the sharper one. A `GET` here would leave what everybody in the house
watches readable by anything on the LAN for as long as the appliance runs.

The open-`GET` principle is untouched. It exists so a *broken* machine can be diagnosed,
and nothing in this endpoint is needed to diagnose one: `/api/status`, `/api/gpu` and
`/healthz` answer as freely as they ever did.

The page enforces this by **not asking**. With no device token in the tab it renders a
locked card and makes no request at all. A page that fetched the titles and then declined
to draw them would have put them in a browser that was never entitled to them.

### Unknown is a value, and it is not "no"

Plex does not populate its hardware-transcoding fields until the transcoder is actually
running. A session captured moments after it started reported `transcodeHwRequested:
false` and `transcodeHwDecodingTitle: "Intel ()"` — indistinguishable from a software
transcode. The same session, seconds later, reported `transcodeHwDecoding: "vaapi"` and
`transcodeHwFullPipeline: true`.

So the model's `hardware` field is three-valued: `true` when Plex names a decoder or
encoder, `false` only when the transcoder has demonstrably done work and named neither,
and `null` otherwise. Collapsing `null` into `false` would put an amber "software
transcode" warning on this appliance's best case every time somebody pressed play.

The same rule governs everything else here. Direct Play is reported from the *absence* of
a transcode session, because that is all Plex says: a direct-play session has no
`TranscodeSession` node and no `decision` field anywhere in it. A field Plex did not
report is a row the page does not draw, never the word "unknown".

### The GPU report and the session answer different questions

`/api/gpu` says whether this machine **can** transcode in hardware. A session says whether
this **stream** is. Both are worth knowing, and the interesting case is when they
disagree — a software transcode on a `ready` GPU usually means the client asked for
something the hardware cannot do, and one on a `degraded` GPU means the GPU is the reason.
The console is the only place both answers exist, so it says which it is rather than
inferring one from the other. Nothing infers "hardware transcode" from a non-zero GPU
clock.

### Observability, never a dependency

Every request to Plex is bounded to two seconds and 512 KiB, and every failure becomes one
of nine states, each naming a remedy: not installed, not running, not claimed, no usable
token, unreachable, refused, unreadable, idle, playing. A parse failure is a state, not a
panic. Nothing in the boot health gate, `/api/status` or the supervisor calls into this
module, so Plex stalling cannot make a boot fail or a status page hang.

Polling is separate from every other poll on the page: three seconds while something is
playing, ten while nothing is, thirty in a background tab, and none at all while the tab
holds no token. The metrics poll fires twice a second and this must not; the status poll
is what every other section depends on and must not acquire a dependency on Plex.

## Consequences

The console answers the question the appliance exists for, and does it without Plex's
credential ever crossing the wire to a browser.

**Titles are now readable by anyone holding the device token.** That token already opens a
root shell (ADR-0014), so it grants nothing new in kind — but it does mean there is no
"can see what is playing but cannot change anything" role, because this console has no
roles at all. One appliance, one administrator (ADR-0013) is unchanged and this is another
thing that decision now covers.

**A stale account token produces a specific, honest state.** Signing the server out of its
Plex account leaves the console reporting that Plex refused the appliance's token, naming
signing in again as the remedy, rather than an empty card.

**What is deliberately not here.** No poster artwork: the browser must not contact Plex
directly, and proxying images is a second decision about a second kind of traffic. No
playback control, no session termination — this is a console that watches, and stopping
somebody's film from a page that also erases disks is a control worth designing on its
own. No library browsing: Plex's own interface does that well and this would be an
imitation of it.

**Field names are pinned to an observed server.** Everything here was read off Plex Media
Server 1.43.3.10861 by driving real sessions through the reference appliance. Plex may
rename any of it; the failure mode is a `null` field or the `unreadable` state, both of
which say so rather than inventing a value.
