# ADR-0013: Authenticating the management console

**Status:** Accepted
**Date:** 2026-07-27

## Context

ADR-0012 accepted an unauthenticated status console on the grounds that it could not
change the machine, and said so with an expiry date attached: *"the moment a route can
change the machine, authentication has to come first."* `plexosd::http` carries a test,
`every_route_is_read_only`, whose comment names the remedy — "the fix is authentication,
not deleting the test."

That moment has arrived. ADR-0010 requires Plex to be provisioned at first boot, and
offline installation to work by the user supplying the official package. Supplying it
through the browser means a route that accepts 83 MB and installs it as system software.
An unauthenticated endpoint that does that is not a console with a rough edge; it is a
remote code execution service with a web page attached.

The constraints are unusual enough that the ordinary answers do not fit:

- **There is no user database and never will be.** One appliance, one administrator.
  Plex manages media accounts itself; this is device-level access.
- **There is no first credential.** The image is identical on every device, so anything
  baked in is a default password — the single most reliably exploited feature of
  consumer appliances.
- **There is no TLS.** The console is plain HTTP on port 80. A certificate needs a name
  and an authority, and a self-signed one on `http://192.168.2.42` trains users to click
  through browser warnings, which is worse than the warning.
- **`/var` is frozen.** ADR-0009 permits a migration only to add, and rollback reverts
  `/usr` and never `/var`, so whatever is stored must be readable by a release that
  predates it.
- **The workspace has almost no dependencies**, and `unsafe_code` is forbidden outside
  `plexos-sys`. A password KDF is not something to implement here.

## Decision

**A single device token, not a username and password.**

256 bits from `/dev/urandom`, generated on the appliance at first boot, displayed on the
console attached to the machine. Physical access is the initial trust: whoever can read
the screen may claim the device. Nothing is baked into the image, so there is no default
credential to leak, publish, or find in a firmware dump.

**Only routes that change the machine require it.** `/`, `/api/status`, `/api/gpu` and
`/healthz` stay open. The console exists to be readable when the machine is broken, and
demanding a token before it will say why a boot failed defeats the reason it was built.
The information it discloses — GPU verdict, health checks, interface, slot — is what
someone on the LAN could largely determine anyway, and none of it is a credential.

**The token is stored hashed, and compared in constant time.** A single SHA-256 is
sufficient and no KDF is required: a KDF exists to make guessing a low-entropy secret
expensive, and there is nothing to guess against 256 bits of entropy. Storing the hash
rather than the token means a copy of `/var` — a backup, a pulled disk — does not yield
something that can be presented to the console.

**No lockout, no rate limiting, no expiry.** All three exist to slow guessing, and
guessing is not a threat here. A lockout would instead be a way for anyone on the LAN to
deny the administrator access to their own appliance.

**The token is rotatable and revocable** by deleting its file: the next start generates
a new one and prints it on the console. That is the recovery path for a token that has
leaked, and it deliberately requires physical access again.

**Plain HTTP is accepted, and recorded as the weakest part of this.** A token sent over
an unencrypted LAN is visible to anything that can see the traffic. This is accepted for
v1 because the alternative on offer — a self-signed certificate — trades a real risk for
a habit of dismissing certificate warnings, which is worse. It is the first thing to
revisit if the console is ever reachable from beyond a LAN, and that condition is the
trigger, not a passage of time.

## Alternatives considered

**Username and password.** Familiar, and what a user expects. Rejected for v1: it needs
a KDF, which means either a dependency or hand-written cryptography, and it needs a
password-setting flow before the machine is usable. A token in a password manager is a
worse experience and a better credential. Revisit if the console grows more than one
kind of user.

**A default password on a sticker or in the documentation.** Rejected outright. Every
device shipping the same image would ship the same credential.

**Deriving a credential from hardware — serial number, MAC address.** Rejected: neither
is secret. A MAC address is broadcast by the machine being protected.

**Plex account sign-in as the console's authentication.** Tempting, since the user will
have an account anyway. Rejected: it makes the appliance's own management depend on a
third party's availability and on outbound internet, and the console's whole purpose is
to work when things are broken. It would also mean the device could not be administered
before Plex is provisioned, which is exactly when it must be.

**Requiring the token for everything, including the status page.** Rejected as
self-defeating. The console replaced transcribing diagnostics off a 2160x1440 panel; a
locked one sends the user back to the panel precisely when they most need not to be
there.

**TLS with a self-signed certificate.** Rejected for v1, see above. A certificate from a
real authority needs a name the appliance does not have.

## Consequences

- The `every_route_is_read_only` test in `plexosd::http` is replaced rather than deleted,
  by one asserting that no route which changes the machine can be reached without a
  valid token. The property being protected is the same; only its shape changes.
- First boot gains a step: the token appears on the console, and the administrator must
  read it off the screen. On a headless install this is a problem, and the removable
  media path (ADR-0010) is what covers that case for provisioning.
- A token displayed on a console is also visible to anyone else who walks past the
  machine while it is on that screen. This is the same trust model as a router's reset
  button, and it is stated here so that it is a decision rather than an oversight.
- Losing the token means physical access to recover, by deleting its file from a shell.
  There is no email reset and nothing to remember.
- `/var` gains one file. It is a single line of text, which any past or future release
  can read or ignore, so it satisfies ADR-0009's rule that a migration may only add.
- Nothing here authenticates *what* is uploaded. A valid token permits installing a
  package; ADR-0010's signature check is what decides whether that package is Plex. The
  two are independent and both are required.
