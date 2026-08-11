# ADR-0019: Pairing a browser from the machine's own screen

**Status:** Accepted
**Date:** 2026-08-11

## Context

ADR-0013 gave this appliance one credential: a device token, 80 bits, generated on the
machine, stored as a fingerprint, printed once on the attached console and typed by hand
into a browser. That decision has held up. There is still no user database, still no
default credential, and a copy of `/var` still yields nothing presentable.

It has been doing two jobs, and it is the right shape for only one of them.

**As a recovery credential it is excellent.** It survives updates and rollbacks, it can be
rotated, and it is the way back into a machine nobody can otherwise reach.

**As the everyday way to get a browser administrating it is poor**, and every part of that
is by construction rather than by accident. Sixteen characters have to be read off a panel
in a console font and typed on another machine. `plexos_sys::tty` reports that panel as
2880x1620 with an 8x16 font, which is text about three millimetres tall. ADR-0013's own
reasoning — "entropy nobody can transcribe is not security, it is an obstacle to the only
person entitled to get past it" — went as far as shortening the token from 64 hex
characters to 16, and stopped there because there was nothing else on offer.

Two things have changed since.

**The console is on TLS and answers at a specific address** (ADR-0014). There is a URL to
put somewhere, and it is a URL only this machine's own screen knows in full.

**The appliance has a screen it does not use.** A monitor plugged into a running MediaLith
shows a scrolling log and a root prompt — which is what a Linux server looks like, and this
is not one. The screen is the one place a credential can be produced that is *provably* in
the room.

The problem is therefore not "the token is bad". It is that there is exactly one credential
where there should be three, distinguished by how long they last and what has to be true to
get one.

## Decision

**Three credentials, and they are not interchangeable.**

| | Recovery device code | Pairing code | Administrator session |
| --- | --- | --- | --- |
| What it is | ADR-0013's device token, renamed in what a person reads | A five-minute, single-use code | An opaque browser credential |
| Entropy | 80 bits | 128 bits | 256 bits |
| Where it lives | SHA-256 on `/var` | Plaintext, in `plexosd`'s memory and on a monitor | SHA-256, in `plexosd`'s memory |
| How long | Until rotated | Five minutes, or one use | 12 h, or 60 min idle |
| Survives a reboot | Yes | No | No |
| Survives a rollback | Yes | No | No |
| How it is obtained | Shown once, at first boot | Pressing **P** on the attached screen | Spending a pairing code |

**The physical action is the security boundary.** Nothing on the network can cause a
pairing code to exist. There is no `POST /api/start-pairing` and there will not be: a route
that produced a credential on request would hand anyone on the LAN the ability to make the
appliance offer one. `pairing::start` is called from the dashboard's keyboard handler and
from the first-boot screen, and from nowhere else.

That is not a new trust model. It is ADR-0013's, stated in the other direction: whoever can
read the attached screen may claim the device. Pressing a key on that screen is a stronger
claim than reading it.

**The QR code carries `https://<address>/#pair=<code>`**, and every part of that is load
bearing:

- **The fragment**, because a fragment is not sent to the server. A query parameter would
  put the code in the request line, and from there into anything that logs one, into the
  browser's history, and into the address bar over the shoulder of whoever is holding the
  phone. The page reads `location.hash`, removes it with `history.replaceState` before its
  first `await`, and sends the code in a `POST` body.
- **`https`**, because that is all this console serves. Port 80 answers a 308 and nothing
  else (ADR-0014).
- **The address** is the first of `reachable_at` — the same list `/api/status` reports and
  the same one the console page tells somebody to type. The dashboard does not choose an
  interface for itself.

**One authority answers "is this an administrator".** `auth::authenticate` consults the
device code and then the sessions, and `http::refusal` calls it once, in front of every
handler. No route knows which credential arrived. That property predates this ADR — it is
what `http`'s module documentation promises — and adding a second way to authenticate is
precisely the change that would have broken it, had it been made by adding a condition to
each handler.

**`POST /api/pair` is the one mutating route that carries no credential**, because it is
where credentials come from. What stands in for authentication is that there is nothing to
spend unless somebody pressed a key at the machine. An appliance nobody has touched answers
every request to it identically, and names the remedy.

**Nothing new is persisted.** Pairing offers and sessions are in one process's memory. That
is the whole of this feature's rollback story: a release rolled back to slot A has no
sessions, needs none, and still accepts the recovery device code, whose file and format are
untouched.

**The screen is a thread inside `plexosd`.** The alternative — a separate renderer talking
to the daemon through `/run` — would need the plaintext code or its fingerprint written to
a file, a mode to protect it, a representation of "expires at" that crosses a process
boundary without depending on the wall clock, and an atomic consume. Every one of those is
a thing to get wrong in exchange for nothing: the daemon is already the process that owns
the credential, and it already runs for the life of the machine under PID 1.

**The log and the console shell move to `/dev/tty2`.** A dashboard and a daemon's log
cannot share a terminal; the log wins, and the result is a designed screen with sentences
through it. Alt+F2 reaches them and Alt+F1 comes back. The shell is exactly as reachable
and exactly as unauthenticated as it has always been — this ADR neither widens it nor adds
a second way in, and no key on the dashboard opens one.

**First boot shows both.** The recovery device code exists in a readable form exactly once,
and that has not changed: `console::claim` hands the plaintext to the dashboard and keeps no
copy. The screen shows it beside a QR code, says it will not be shown again, and drops it
from memory when any key is pressed.

**Rotating the recovery device code revokes every session and cancels any offer.** Rotation
is what somebody does when a credential has got out; leaving browsers admitted under the old
one still administering would make it a password change that logs nobody out. It signs out
the browser that asked, which is deliberate — anything else means deciding that one session
is special, and the only way to know which is to trust the request that arrived.

**No lockout, no rate limit.** ADR-0013's reasoning applies unchanged and with more force:
against 128 bits there is nothing to guess, and a rule that invalidated the offer on a wrong
attempt would give anyone who can reach the port a way to stop the owner pairing.

## Revision, 2026-08-11: one authorised browser approving another

The decision above answers "how does the *first* browser get in". The question that follows
it, an hour later, at a desktop on the other side of the house, was left with the answer
this ADR was written to remove: fetch the recovery device code and type sixteen characters.

**A browser that is already an administrator may approve one that is not.** The desktop
asks, the phone approves, and MediaLith issues the desktop a session of its own.

### Three ways in, and they are different chains

| | What is trusted | What it produces |
| --- | --- | --- |
| **Physical console pairing** | Somebody is at the machine and pressed a key on it | An administrator session |
| **Browser approval** | Somebody who is *already* an administrator said yes | A **separate** administrator session |
| **Recovery device code** | Somebody knows the credential this machine printed once | Authentication directly, and may also approve |

The second is the new one, and it is a delegation rather than a third kind of credential:
the authority it spends is authority the appliance already granted, and what it produces is
an ordinary [`session`] with the ordinary deadlines.

### The phone is not a relay

The obvious implementation hands the phone's session token to the desktop. It works, and it
is wrong: a credential that travels between browsers exists in two places, cannot be revoked
in one of them, and turns "sign this desktop out" into a question about which copy.

What the phone sends is a sentence — *I approve request X*. Everything else is the
appliance's own work: it mints the session, when the desktop redeems, out of the same store
every other session comes from. There is no mechanism here for moving a session, and the
tests assert the absence at the two boundaries where somebody might add one.

### Two values, because a monitor is a public place

A request has an **id**, which travels in the QR on the desktop's screen, and a **secret**,
which never leaves the desktop that asked. Redeeming needs both.

Collapsing them into one QR secret would be simpler, would look identical in every
demonstration, and would hand the session to whoever photographed the monitor first. With
the split, somebody who watched the entire exchange — the QR, the approval, the redemption
— still holds one half of two.

The secret is also what authorises cancelling, which is why cancelling needs no
administrator: only the browser that asked can have it. Without that, an id read off
somebody's screen would let a passer-by stop their pairing, repeatedly, from anywhere on the
network.

### Anybody may ask; only an administrator may answer

`POST /api/browser-pair/start` carries no credential, because asking is not being let in: it
creates a request that does nothing until an administrator approves it. `inspect`, `approve`
and `deny` are `POST`s, so the existing gate has already demanded an administrator before
they are reached — **no new authentication logic exists in this feature.**

A browser holding the recovery device code may approve as well as one holding a session.
That follows from ADR-0013 rather than being decided here: the recovery code already
authorises installing an operating system and opening a root shell, so withholding "may let
another browser in" from it would be a distinction with nothing behind it. It falls out of
using `auth::authenticate` rather than being written anywhere.

### The verification code is not a secret

Four digits, derived from the request id, shown on both screens. The id is in a QR on a
monitor, so anybody who could compute this could already have read it. It exists for the
mistake a person actually makes — approving the wrong request, because two are in flight or
because the phone shows one thing and the desk another. Four digits is a length somebody
will genuinely compare; sixteen characters is a length they will glance at and assume.

### What is deliberately not here

**The requesting browser's IP address**, which the approval screen would have shown. The
request path does not carry the peer address, and plumbing it touches thirty struct literals
in the code that parses untrusted input — for a field that is informational by this ADR's
own rules, since redemption must not be bound to an address that changes when a phone moves
between access points. The verification code does the job it would have done, better.

**Any memory of an approved browser.** No trusted-device list, no certificates, no
"remember this computer". A browser approved yesterday whose tab has closed is a browser
that asks again — which is the same property the sessions themselves have, and the reason
there is nothing here to revoke separately.

## Alternatives considered

**A code typed off the screen instead of scanned.** That is the device token, which is what
this exists to stop needing.

**Bluetooth or mDNS discovery with a confirmation on the screen.** Better on paper and much
worse here: it needs a stack this image does not have, a pairing protocol to write, and it
moves the trust boundary from "in the room" to "in radio range".

**A JWT instead of an opaque session token.** A JWT exists so that a verifier can avoid a
lookup, which matters when the verifier is not the issuer. Here they are the same process
with at most sixteen sessions in a `Vec`. It would add a signing key, a clock dependency and
a parser for attacker-supplied structure, and it would remove the one property that matters:
a signed token cannot be revoked without the table it was meant to replace.

**Half-block characters for the QR code**, which would halve the rows needed. Rejected
because they depend on the console font carrying U+2580, and a missing glyph renders as a
blank cell — so the failure looks like a broken feature rather than like a font, and nothing
in this repository could see it. Modules are spaces with a background colour instead.

**Handing the approving browser's session to the approved one.** Considered and rejected
above; recorded here because it is what almost every implementation of this does, and
because it is invisible in testing — the desktop ends up authenticated either way.

**A trusted-device list, so a desktop is approved once and remembered.** Rejected: it is a
persistent credential wearing different clothes, it needs storage that survives a rollback,
and it turns a five-minute question into a database somebody has to be able to audit and
prune. The session already lasts twelve hours, which is the length of a day at a desk.

**One QR secret instead of two values.** Rejected, at length, above.

**Showing the QR code permanently.** Rejected: a usable credential standing on a screen is
one anybody who walks past can photograph, and the physical action is the entire security
argument. The exception is first boot, where the person who just turned the machine on is
standing at it, and the offer is made once rather than renewed.

**A framebuffer or kiosk UI.** Rejected without much thought. It would mean a graphics
stack, a browser or a toolkit in an image whose defining property is that it is one signed
artefact, in exchange for a prettier version of eleven lines of text.

## Consequences

- A new dependency, `qrcode`, with `default-features = false` and **no transitive
  dependencies at all**. ISO/IEC 18004 is Reed–Solomon over GF(256), eight mask patterns
  scored against four penalty rules and a version table; a subtle mistake in any of it
  produces a symbol that looks plausible and does not scan.
- `plexos-sys` gains a `tty` module: `TIOCGWINSZ` and two `termios` calls. It is the only
  crate allowed `unsafe`, which is why it is there rather than in `plexosd`.
- The attached screen is no longer a log. Somebody who wants one presses Alt+F2. This is
  the change most likely to surprise anybody who has used this appliance before, and it is
  written on the dashboard's own help screen.
- The dashboard writes nothing when its content has not changed, and nothing at all after a
  minute with no keypress, so that the kernel's blank timer can still turn the panel off.
  A field that ticked every second would have undone that silently — which is why the
  uptime on that screen is deliberately coarse.
- The console page now has two credentials to hold, and one function that decides which to
  send. A tab holding both sends the session, because that is the one the person just
  established and the one "Sign out" can end.
- ADR-0013 is not superseded. Its credential, its file, its format and its reasoning are
  unchanged; what changes is that it is no longer the only way in, and in what a person
  reads it is called the **recovery device code**.
