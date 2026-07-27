# ADR-0012: A read-only status console, served by `plexosd`

**Status:** Accepted
**Date:** 2026-07-27

## Context

Everything PlexOS reports, it reports to a kernel console. That was adequate while the
only question was "did it boot", and it stopped being adequate as soon as the answers
got long. The reference laptop has a 2160x1440 panel, no usable serial port, and no
way to copy text off the screen: reading a diagnostic means transcribing it by hand or
photographing it. `plexos-gpu` alone can emit a dozen lines of findings, each with a
remedy, and those are precisely the lines a person needs to act on.

There is a second problem, and it is the one that motivated this now. The project's
central question — does hardware transcoding work on this machine — is answered by a
tool whose output nobody can conveniently read. A verdict that is technically produced
but practically inaccessible has not been delivered.

`plexosd` was always intended to grow a management API; its own module documentation
has said so since it was written. What it did not have was any reason to grow one
first, because there was nothing to manage. There is now something to *show*, which is
a smaller thing and a good place to start.

The constraint that shapes the whole design comes from ADR-0005 by way of
`health.rs`: **no part of the boot health gate may depend on the network.** Ethernet
arrives over USB on this hardware, USB enumerates seconds after PCI, and a gate that
waited for an address would roll back a perfectly good update because a dongle was
slow.

## Decision

**`plexosd --serve` brings the network up and serves a read-only status console over
HTTP.** `plexos-init`'s supervisor spawns it after the health gate has returned its
verdict, and before the shell.

**It shows; it does not do.** Every route is read-only. The HTTP layer answers any
method but `GET` and `HEAD` with 405, and a test enforces that. There is no
authentication, and that is defensible only while the previous sentence is true.

**The ordering is structural, not remembered.** The network is configured by the
daemon that runs after the boot decision, so there is no code path from network
bring-up to `Health::is_healthy`. The rule from ADR-0005 cannot be violated by someone
who has not read it.

**Wired Ethernet only.** `linux.fragment` already decided v1 is wired USB Ethernet and
ships no `wpa_supplicant`; the interface selector skips wireless rather than running a
DHCP client on an interface that can never associate. It also skips virtual interfaces
— bridges and `veth` pairs are `ARPHRD_ETHER` and carry a carrier, so type alone
cannot distinguish them from a network card.

**No web framework, and no external assets.** The server is written against
`std::net`; the page is a single file embedded with `include_str!`.

**Port 80, on all interfaces.** The point is that someone types an address and gets a
page.

## Alternatives considered

**Write diagnostics to the ESP and read them from another machine.** This works today
and is how boots have been verified so far — the ESP is FAT and Windows can read it.
It was rejected as the primary answer because it requires physical access and a
reboot to see anything, and because it cannot show a machine's *current* state, only
what it recorded when it last ran. It remains the right tool for post-mortems and is
not replaced by this.

**SSH and a terminal.** More capable, and a much larger surface: a shell on the
appliance is the one thing an unauthenticated LAN service must never approach. It also
does not answer the question a person actually has, which is "is it working", not
"give me a prompt".

**A web framework.** Rejected for the reason `plexos-sys` rejected `rustix`: this
binary ships inside a verity-sealed, read-only `/usr` on an appliance that must keep
working with nothing to go stale, and the dependency tree would be orders of magnitude
larger than the handful of read-only routes it serves. The cost is accepted explicitly
— the server speaks a small subset of HTTP/1.1, with no keep-alive, no chunked
encoding, no compression and no TLS.

**Authentication now.** Rejected as premature while nothing can be changed, and
recorded here as a debt rather than a decision: the moment a route grows a verb, this
ADR is superseded.

**Bringing the network up in `plexos-init`.** Rejected precisely because it would put
network code on the boot path, where the next person to add a "wait until ready" would
reintroduce the rollback bug ADR-0005 exists to prevent.

## Consequences

- A person can read the GPU verdict, the health gate and the network state from a
  phone, without transcribing anything. This is the first part of PlexOS a user
  interacts with rather than reads.
- **The console is unauthenticated and reachable by anyone on the LAN.** It discloses
  the OS version, slot, dm-verity root hash, MAC addresses and hardware inventory.
  None of it is a credential; all of it is reconnaissance. This is accepted for a home
  appliance and must be revisited before anything ships publicly.
- `plexosd` gains a second, long-running role. It is no longer only the thing that
  runs once at boot and exits, which is a change in what the name means.
- Nothing here reports whether the current slot has been marked permanent. Answering
  that means mounting the ESP, and doing so from an HTTP handler would race the one
  write in the system that decides whether the machine rolls back.
- The status document is a wire format in the weak sense: the page and the API are
  versioned together inside one binary, so they cannot drift. Anything *outside* the
  image that starts parsing `/api/status` acquires a compatibility obligation this ADR
  does not grant it.
- `plexos-gpu` is now a library dependency of `plexosd` rather than a binary it
  shells out to.
