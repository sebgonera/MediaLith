# ADR-0014: A terminal in the console, on a trusted network

**Status:** Accepted
**Date:** 2026-07-29

## Context

Administering this appliance still means a shell, and reaching a shell still means the
attached screen. Every fault of the last month was diagnosed that way: a 2160x1440 panel,
a keyboard borrowed from somewhere, and somebody transcribing output by hand. The status
console has removed most of the *reading* — `/api/status`, `/api/update`, `/api/network`
and the provisioning log all answer over HTTP — but nothing removes the *doing*.

Two questions had to be settled before any code, and one of them changes ADR-0013's
threat model rather than extending it.

**What carries the bytes.** The console's HTTP server is hand-written and answers
request/response. A terminal needs a long-lived, two-directional channel.

**What protects it.** A terminal is a root shell. The console runs over plain HTTP on the
LAN, and ADR-0013's device token is a bearer credential sent in a header — it stops
somebody who wanders onto the network, and it does not stop somebody who can read the
wire.

## Decision

### Transport: a long-polled output route, plus a route for keystrokes

Output is fetched by a `GET` that blocks until there is something to send or a deadline
passes, and returns everything produced since a byte offset the client supplies. Input is
a separate `POST` per keystroke batch. Both carry a session identifier.

Long-polling rather than a chunked stream, and that is a correction to the first draft of
this ADR: the console's `Response` carries a `Vec<u8>` body and has no way to hand a
handler the socket, so a streaming response would have meant changing the HTTP server
itself. It did not need to. The server is thread-per-connection, so a handler may simply
block, and a wait bounded below `IO_TIMEOUT` gives output that arrives as soon as it
exists — the property a stream was wanted for — with no new machinery in the layer that
parses untrusted requests.

The byte offset is what makes this correct rather than merely convenient. A client that
asked "what is new" would lose whatever arrived between two polls; a client that asks
"everything after byte N" cannot.

**Not a WebSocket.** Nothing in this image provides one, so it would mean hand-writing the
handshake — a SHA-1 of the client key concatenated with a fixed GUID — and the framing
layer, including client-to-server masking, continuation frames, control frames and close
codes. That is several hundred lines of parser sitting directly behind a root shell, in a
project whose HTTP server is already hand-written and whose author has no way to fuzz it.
The gain over two ordinary HTTP requests is latency measured in milliseconds on a link
where the user is typing.

The cost is honest and worth stating: one connection is held open per session, and input
carries an HTTP request's overhead per batch. For one administrator on a LAN, neither
matters.

### Security: this console assumes a trusted network, and says so

The terminal is served over plain HTTP, gated by the ADR-0013 device token, and the
project documents that **the management console is fit for a trusted LAN and is not fit to
expose to the internet or to an untrusted network.**

This is a change of posture, not a discovery. ADR-0013 reasoned about a passer-by with a
browser. A root shell means the threat model must now be stated in full:

- The token is a bearer credential in cleartext. Anyone who can observe traffic between
  the browser and the appliance obtains it, and with it a root shell.
- The token already authorises installing an operating system and powering the machine
  off. A shell increases the *convenience* of an attack that the token already permits;
  it does not create a capability that was absent.
- Nothing signs update bundles yet (ADR-0006 is unfinished), so an attacker on the wire
  can already choose what `/usr` this appliance runs.

The last two points are why TLS was not made a precondition. Adding TLS to this console
while the update path remains unsigned and unauthenticated would protect the smaller
opening and leave the larger one, and would read as a security guarantee the system does
not provide.

**TLS is deferred, not rejected.** When ADR-0006 is finished, a self-signed certificate
with its fingerprint printed on the attached screen is the intended next step, and it
belongs in the same piece of work as making the console safe to reach from outside the
LAN. Until then, the page says what it is.

### The session

One session at a time. Two administrators sharing one PTY is a feature nobody asked for
and a source of confusing state; a second request is refused with the reason.

A session ends when its output stream closes, when the shell exits, or after an idle
timeout. The timeout matters more than it looks: a browser tab closed without ceremony
leaves a root shell running, and "it goes away when you close the tab" is not something a
hand-written HTTP server can be relied upon to notice promptly.

The shell runs as root, in the machine's own namespaces, with no Landlock policy. It is a
recovery and administration tool, and confining it would produce a shell that cannot do
the thing anyone opened it for. This is precisely why the paragraph above about the
network matters.

## Alternatives considered

**A hand-written WebSocket.** Rejected above: several hundred lines of unfuzzed parser
behind a root shell, for milliseconds.

**Exposing a fixed set of operations instead of a shell** — restart Plex, view a log, run
a diagnostic. Genuinely safer, and the console already does this where the operation is
known in advance. Rejected as a *replacement* because the operations that have actually
been needed were not known in advance: every one of the last month's faults was diagnosed
by looking at something nobody had thought to expose. A terminal is the admission that the
list cannot be complete.

**SSH.** The obvious answer, and it would bring proper transport security. Rejected for
now: it means a server, host keys, key distribution and an account model, none of which
this appliance has, and it does not remove the browser as the thing an administrator
actually has open. Worth revisiting — it is a better answer than TLS-plus-fingerprint if
the console ever needs to be reachable from outside a LAN.

**Requiring TLS first.** Rejected as sequencing, not as direction. See above: it would
close the smaller hole while the unsigned update path stands open, and imply a guarantee
that is not there.

## Revised 2026-07-30: TLS is no longer deferred

The condition this ADR set has been met. ADR-0006 is finished and proven on hardware — an
appliance now refuses an unsigned bundle, a tampered manifest, a replayed release and a
revoked key — so closing the console no longer protects the smaller opening while the
larger one stands.

**The console serves HTTPS and nothing else.** Port 80 answers every request with a 308 to
the same path on 443 and does nothing else; it never reads a body and never looks at a
credential, because anything that reached it was already sent in clear and the only useful
thing left is to say where the encrypted door is.

Serving *only* TLS was a deliberate choice over serving both. Keeping cleartext alongside
would mean the device token still travels in the open for anyone who types `http://`, which
is the whole thing this closes. The cost is equally deliberate and worth stating: **if the
TLS path fails to start, the console is gone**, and the ways back are the attached screen or
three power cycles into ADR-0005's rollback. That risk was accepted knowingly.

**The certificate is self-signed, and the key outlives it.** There is no CA and no domain
name; the address comes from DHCP and moves. So the certificate names whatever addresses the
machine currently has and is reissued when they change, while the key is generated once and
kept — and the fingerprint reported everywhere is the key's. A fingerprint that changed with
every lease would teach exactly one lesson: that the warning means nothing.

**The tension this ADR recorded as unresolved is still unresolved.** The first comparison of
that fingerprint has nowhere to happen but the attached screen, which is the thing the
console exists to stop needing. What has changed is only that the comparison is now
*possible*: the fingerprint is printed at boot and served at `/api/status`, so somebody who
wants to check can, and somebody who does not is still protected from anyone merely
listening. It does not protect against an active middle, and the page says so in those
words rather than claiming more.

**rustls, not the OpenSSL already in the image.** Not a preference: `plexosd` is a static
binary built by the workspace's own toolchain, linking nothing from Buildroot's sysroot, and
its install step refuses a dynamic one. Pure Rust keeps that true. Found while planning it,
and worth recording: the image carries `libssl` and `libcrypto` for curl but **no `openssl`
binary**, so a certificate could not have been generated by a script even if the linkage had
worked.

## Consequences

- **The project now has a documented network boundary.** "Trusted LAN only" has to appear
  in the console, the README and the first-boot experience, not only in this file. A
  security posture recorded exclusively in an ADR is one that users do not have.
- ADR-0013's device token becomes root-equivalent in practice. Rotating it needs to be
  possible without a shell — which is currently the one operation that still requires
  the attached screen, and is now more clearly a gap than it was.
- The idle timeout is a safety property, not a nicety, and belongs in tests.
- `plexos-sys` gains PTY allocation: `openpty`, `setsid`, `TIOCSCTTY` and window sizing.
  That is more `unsafe` in the one crate allowed to have it, and each block needs its
  soundness argument like the rest.
- When TLS arrives, the fingerprint has to be shown somewhere a user can compare it, and
  the only such place on this hardware is the attached screen — the thing this whole
  console exists to stop needing. That tension is real and unresolved.
