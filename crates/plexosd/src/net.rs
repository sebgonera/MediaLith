//! Bringing the network up, and keeping it away from the health gate.
//!
//! # Why this is in `plexosd` and not in `plexos-init`
//!
//! [`health`](crate::health) states the rule this module has to obey: **no part of the
//! boot gate may depend on the network.** Ethernet arrives over USB on the reference
//! machine, USB enumerates seconds after PCI, and a gate that waited for an address
//! would roll back a perfectly good update because a dongle was slow. So the network
//! cannot be brought up on the path that decides whether the boot was good.
//!
//! Putting it here, in the daemon that runs *after* the gate has already returned its
//! verdict, makes that structural rather than a rule someone has to remember. There is
//! no code path from this module to [`Health::is_healthy`](crate::health::Health::is_healthy).
//!
//! # The ordering that is not obvious
//!
//! `carrier` in sysfs is not readable until the interface is administratively up — the
//! kernel returns `EINVAL` on a down interface, not `0`. So "wait for a cable" cannot
//! come first: every candidate has to be brought up *before* its carrier means
//! anything, and [`Interface::carrier`] is `false` for both "no cable" and "not up
//! yet".
//!
//! The consequence is easy to get wrong, and the first version of this module got it
//! wrong on the reference laptop. Bringing the links up **once, before the wait**, is
//! not enough: the interface being waited for is the one that arrives late, so at the
//! moment of that single pass it does not exist to be brought up. It then sits
//! administratively down for the whole timeout, its `carrier` unreadable and therefore
//! `false`, and the wait expires against an adapter that was plugged in the entire
//! time. [`wait_for_link`] therefore brings candidates up **on every pass**, not once
//! before the loop.
//!
//! # What is verified and what is not
//!
//! Discovery, classification, selection and address parsing are pure functions over
//! [`Environment`], and are tested against recorded sysfs.
//!
//! This module **has** now run on the reference laptop, and the run is what found the
//! bug above. The immutable [`Fixture`](plexos_gpu::env::Fixture) could not have: it
//! describes a machine whose interfaces are all present from the start, which is the
//! one case that never happens when Ethernet arrives over USB. The regression test
//! uses a small `Environment` that enumerates the adapter late and refuses to report a
//! carrier until something has actually brought it up — the kernel's own semantics,
//! rather than this module's assumptions about them.
//!
//! The whole path — enumeration, bring-up, `udhcpc`, lease, address — has now run on
//! the reference laptop and produced an address the console was reached on from another
//! machine. What remains untested rather than unverified is the `udhcpc` launch itself:
//! it is one process spawn, and a test that mocked it would compare this module to its
//! own assumptions, which is exactly how the three faults above survived a full suite.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use plexos_gpu::env::Environment;

/// Where the kernel lists network interfaces.
const SYS_CLASS_NET: &str = "/sys/class/net";

/// `ARPHRD_ETHER`, from `include/uapi/linux/if_arp.h`. Pinned against the kernel
/// header rather than inferred from an interface name: `eth0` is a convention, this
/// is the kernel's own answer. If this test fails, the constant is what changes.
const ARPHRD_ETHER: u32 = 1;

/// `ARPHRD_LOOPBACK`, from the same header.
const ARPHRD_LOOPBACK: u32 = 772;

/// `IFF_UP`, from `include/uapi/linux/if.h`, where it is `1<<0` in `net_device_flags`.
///
/// Read from sysfs `flags` rather than inferred from `operstate`, because the two
/// answer different questions. `operstate` is `down` both for an interface nobody has
/// brought up and for one that is up with no cable in it; only `IFF_UP` distinguishes
/// them, and that distinction is the difference between "run `ip link set up`" and
/// "check the cable".
const IFF_UP: u32 = 1 << 0;

/// How long to wait for a wired interface to appear and acquire a carrier.
///
/// Matches [`plexos_sys::device::DEVICE_TIMEOUT`] deliberately: it is the same USB
/// enumeration delay being waited out, measured on the same machine, where at 12.9 s
/// into boot only a hub had appeared.
pub const LINK_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between rescans while waiting for a link.
const POLL: Duration = Duration::from_millis(500);

/// Where the programs this module runs actually live, in search order.
///
/// Naming a program and letting the system find it does not work here, and the way it
/// fails is quiet. `plexosd` is exec'd by `plexos-init`, which is PID 1 and inherits
/// the empty environment the kernel gives it, so there is no `PATH`. glibc's `execvp`
/// then falls back to `confstr(_CS_PATH)`, which is `/bin:/usr/bin` — verified with
/// `getconf PATH` — and busybox installs `ip` and `udhcpc` into `/sbin` and
/// `/usr/sbin` only. So the lookup that works when a person types it at the shell,
/// which sets its own `PATH`, fails from the daemon with a bare `ENOENT` naming the
/// program and nothing about why.
///
/// Resolving against this list instead means the result does not depend on who started
/// the process.
const PROGRAM_DIRS: [&str; 4] = ["/sbin", "/usr/sbin", "/bin", "/usr/bin"];

/// Finds `program` as an absolute path, ignoring `PATH` entirely.
///
/// See `PROGRAM_DIRS` for why. Returns `None` when it is in none of them, which is an
/// image problem rather than a runtime one — busybox is supposed to provide both.
pub fn resolve(env: &impl Environment, program: &str) -> Option<String> {
    PROGRAM_DIRS.iter().find_map(|dir| {
        let candidate = PathBuf::from(dir).join(program);
        env.list_dir(std::path::Path::new(dir))
            .ok()?
            .contains(&candidate)
            .then(|| candidate.to_string_lossy().into_owned())
    })
}

/// What kind of interface the kernel says this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Ethernet. The only kind this release configures.
    Wired,
    /// Wireless. Skipped — see [`Kind::Wireless`] handling in [`candidates`].
    Wireless,
    /// Loopback. Always present, never configured.
    Loopback,
    /// Something else: tunnels, bridges, virtual devices.
    Other,
}

/// One network interface, as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    /// Kernel name, e.g. `eth0`.
    pub name: String,
    /// What it is.
    pub kind: Kind,
    /// Whether a cable is detected. `false` also means "not up yet" — see the module
    /// documentation.
    pub carrier: bool,
    /// Whether the interface is administratively up (`IFF_UP` in sysfs `flags`).
    ///
    /// This is the precondition for [`Interface::carrier`] meaning anything at all, so
    /// it is also what tells "nothing has brought this up" apart from "no cable".
    pub up: bool,
    /// The kernel's `operstate`, e.g. `up`, `down`, `unknown`.
    pub operstate: String,
    /// MAC address, if the kernel reports one.
    pub mac: Option<String>,
    /// Whether a real device backs this interface, rather than the kernel alone.
    ///
    /// Bridges, `veth` pairs, tunnels and `docker0` are all `ARPHRD_ETHER` and can all
    /// carry a carrier, so type alone cannot tell them from a network card. Only an
    /// interface backed by a bus device has a `device` symlink in sysfs, and that is
    /// the kernel's own answer to the question rather than a guess from the name.
    pub physical: bool,
}

/// An address currently configured on an interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// The interface it belongs to.
    pub interface: String,
    /// The address in CIDR form, e.g. `192.168.2.42/24`.
    pub cidr: String,
}

impl Address {
    /// The address without its prefix length, for printing a URL.
    #[must_use]
    pub fn ip(&self) -> &str {
        self.cidr.split('/').next().unwrap_or(&self.cidr)
    }
}

/// Classifies an interface from its `type` and `uevent`.
///
/// Wireless is identified by `DEVTYPE=wlan` in `uevent` rather than by the presence of
/// a `wireless/` directory: both work, but only one is a single file read, and this
/// crate's whole test strategy is built on file reads.
#[must_use]
pub fn classify(if_type: &str, uevent: &str) -> Kind {
    if uevent
        .lines()
        .any(|line| line.trim() == "DEVTYPE=wlan" || line.trim() == "DEVTYPE=wwan")
    {
        return Kind::Wireless;
    }
    match if_type.trim().parse::<u32>() {
        Ok(ARPHRD_LOOPBACK) => Kind::Loopback,
        Ok(ARPHRD_ETHER) => Kind::Wired,
        _ => Kind::Other,
    }
}

/// Reads one interface out of sysfs.
///
/// Absent optional attributes are not errors: `carrier` is unreadable on a down
/// interface, and a device with no MAC is unusual rather than broken.
fn read_interface(env: &impl Environment, name: &str) -> io::Result<Interface> {
    let base = PathBuf::from(SYS_CLASS_NET).join(name);
    let if_type = env.read(&base.join("type"))?;
    let uevent = env.read(&base.join("uevent")).unwrap_or_default();

    Ok(Interface {
        name: name.to_owned(),
        kind: classify(&if_type, &uevent),
        carrier: env
            .read(&base.join("carrier"))
            .is_ok_and(|c| c.trim() == "1"),
        // sysfs writes this as hex with an `0x` prefix, e.g. `0x1003`. An unreadable
        // or unparsable value counts as not up, which costs one redundant `ip link
        // set up` and never a missed one.
        up: env
            .read(&base.join("flags"))
            .ok()
            .and_then(|f| u32::from_str_radix(f.trim().trim_start_matches("0x"), 16).ok())
            .is_some_and(|flags| flags & IFF_UP != 0),
        operstate: env
            .read(&base.join("operstate"))
            .map_or_else(|_| "unknown".to_owned(), |s| s.trim().to_owned()),
        mac: env
            .read(&base.join("address"))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
        physical: env.read_link(&base.join("device")).is_ok(),
    })
}

/// Every interface the kernel currently knows about, in name order.
///
/// # Errors
/// Fails only if `/sys/class/net` itself cannot be listed, which means sysfs is not
/// mounted — a much larger problem than a missing network.
pub fn interfaces(env: &impl Environment) -> io::Result<Vec<Interface>> {
    let mut found = Vec::new();
    for entry in env.list_dir(std::path::Path::new(SYS_CLASS_NET))? {
        let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // An interface that vanishes mid-scan is normal while USB settles. Skip it
        // rather than failing the whole enumeration.
        if let Ok(interface) = read_interface(env, name) {
            found.push(interface);
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// The interfaces worth configuring: real, wired hardware.
///
/// Wireless is excluded because this image cannot use it. `CONFIG_IWLWIFI` is built
/// in, but there is no `wpa_supplicant` in the rootfs, so an encrypted network cannot
/// be joined — and running a DHCP client on an unassociated `wlan0` would wait out the
/// full timeout for nothing. `buildroot/board/plexos/x86_64/linux.fragment` records
/// the decision that wired USB Ethernet is the supported configuration for v1.
///
/// Virtual interfaces are excluded because they are indistinguishable from hardware by
/// type: a bridge, a `veth` and `docker0` are all `ARPHRD_ETHER` and all report a
/// carrier. On a machine running containers they sort before the real card by name,
/// and DHCP would be run on a bridge that goes nowhere. The appliance has none of them
/// today, which is exactly why this is worth encoding now rather than after something
/// adds one.
#[must_use]
pub fn candidates(all: &[Interface]) -> Vec<&Interface> {
    all.iter()
        .filter(|i| i.kind == Kind::Wired && i.physical)
        .collect()
}

/// The interface to run DHCP on: a wired one with a carrier, first by name.
///
/// Returns `None` when nothing is plugged in, which is a normal state and not an
/// error — the appliance is expected to boot with no cable and be useful over the
/// console.
#[must_use]
pub fn preferred(all: &[Interface]) -> Option<&Interface> {
    candidates(all).into_iter().find(|i| i.carrier)
}

/// Parses `ip -o -4 addr show`.
///
/// One line per address, in the form
/// `2: eth0    inet 192.168.2.42/24 brd ... scope global eth0\       valid_lft ...`.
#[must_use]
pub fn parse_addresses(output: &str) -> Vec<Address> {
    let mut addresses = Vec::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        // `1:` then the interface name.
        let Some(_index) = fields.next() else {
            continue;
        };
        let Some(interface) = fields.next() else {
            continue;
        };
        let mut rest = fields.skip_while(|f| *f != "inet");
        if rest.next().is_none() {
            continue;
        }
        if let Some(cidr) = rest.next() {
            addresses.push(Address {
                interface: interface.trim_end_matches(':').to_owned(),
                cidr: cidr.to_owned(),
            });
        }
    }
    addresses
}

/// The addresses currently configured, excluding loopback.
///
/// Loopback is dropped because the one question this answers is "what should I type
/// into a browser", and `127.0.0.1` is never that answer from another machine.
#[must_use]
pub fn addresses(env: &impl Environment) -> Vec<Address> {
    // Absolute, for the reason in PROGRAM_DIRS. Getting this wrong here is quieter than
    // anywhere else in the module: the console still serves, it just never prints the
    // address a person is supposed to type.
    let Some(ip) = resolve(env, "ip") else {
        return Vec::new();
    };
    let Ok(output) = env.run(&ip, &["-o", "-4", "addr", "show"]) else {
        return Vec::new();
    };
    parse_addresses(&output)
        .into_iter()
        .filter(|a| a.interface != "lo")
        .collect()
}

/// How long to wait for DHCP to produce an address, once the link is up.
///
/// Shorter than [`LINK_TIMEOUT`] because a different thing is being waited for. That one
/// waits out USB enumeration, measured in tens of seconds; a DHCP server on the same
/// segment answers in one or two, and if it has not answered in fifteen it is not going
/// to.
pub const LEASE_TIMEOUT: Duration = Duration::from_secs(15);

/// Waits for an IPv4 address to appear on `interface`.
///
/// Separate from [`configure`] because `udhcpc` is spawned and never waited on — it
/// stays resident to renew the lease, so the only way to learn that an address arrived
/// is to look for it.
///
/// Without this the console printed its URL exactly once, at the instant udhcpc was
/// started, which is always before any lease exists. The single line a person actually
/// needs — the address to type into a browser — was therefore never shown, on a machine
/// whose networking was working.
pub fn wait_for_address(
    env: &impl Environment,
    interface: &str,
    timeout: Duration,
    log: &mut dyn FnMut(&str),
) -> Option<Address> {
    let deadline = Instant::now() + timeout;
    let mut announced = false;

    loop {
        if let Some(found) = addresses(env)
            .into_iter()
            .find(|a| a.interface == interface)
        {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        if !announced {
            announced = true;
            log("waiting for a DHCP lease");
        }
        sleep(POLL);
    }
}

/// Describes one interface for the timeout message, in the terms that pick the remedy.
///
/// `operstate` alone is not enough: it reads `down` both for an interface nothing has
/// brought up and for one that is up with no cable, and those need opposite responses.
fn describe(interface: &Interface) -> String {
    let state = match (interface.up, interface.carrier) {
        (false, _) => "not up",
        (true, false) => "up, no carrier",
        (true, true) => "up, carrier",
    };
    format!("{} ({state})", interface.name)
}

/// Waits for a wired interface to appear and report a carrier, bringing candidates up
/// as they appear.
///
/// Polls, because all three of the conditions arrive late and independently: the device
/// appears when USB enumerates, it becomes readable once something brings it up, and
/// the carrier appears when the link negotiates. Announces the wait exactly once, and
/// only when there is actually a wait, so a machine with the cable already up logs
/// nothing.
///
/// The bring-up happens **inside** the loop. See the module documentation: doing it
/// once beforehand cannot work, because the interface being waited for is precisely
/// the one that is not there yet when that single pass runs.
///
/// # Errors
/// Times out. That is not a boot failure — see [`configure`].
pub fn wait_for_link(
    env: &impl Environment,
    timeout: Duration,
    log: &mut dyn FnMut(&str),
) -> io::Result<Interface> {
    let deadline = Instant::now() + timeout;
    let mut announced = false;
    let mut warned = BTreeSet::new();

    // Once, not per pass: the answer cannot change during a boot, and listing four
    // directories twice a second for thirty seconds is pure waste.
    let ip = resolve(env, "ip");

    loop {
        let all = interfaces(env).unwrap_or_default();
        if let Some(found) = preferred(&all) {
            return Ok(found.clone());
        }

        // Before the deadline check, so that an interface appearing on the very last
        // pass is still brought up rather than reported as "not up" and abandoned.
        match ip.as_deref() {
            Some(ip) => link_up(env, &all, ip, &mut warned, log),
            // Worth saying only once something needs raising. A machine still waiting
            // for its adapter to enumerate has nothing to run `ip` against yet, and
            // announcing a missing tool before there is any work for it is noise.
            None if candidates(&all).iter().any(|i| !i.up) => {
                if warned.insert("ip".to_owned()) {
                    log(&format!(
                        "`ip` is in none of {}. Nothing can bring an interface up, so no \
                         carrier will ever be readable and this wait can only time out. \
                         That is an image fault: busybox provides the applet and \
                         Buildroot should have linked it.",
                        PROGRAM_DIRS.join(", ")
                    ));
                }
            }
            None => {}
        }

        if Instant::now() >= deadline {
            let wired = candidates(&all);
            let seen = all
                .iter()
                .filter(|i| i.kind != Kind::Loopback)
                .map(describe)
                .collect::<Vec<_>>()
                .join(", ");
            let seen = if seen.is_empty() {
                "none".to_owned()
            } else {
                seen
            };
            // Match the remedy to the state actually reached. Telling someone to check
            // a cable that is plugged in is how the first version of this wasted an
            // evening: the adapter was connected throughout and the fault was here.
            let remedy = if ip.is_none() {
                "`ip` could not be found, so nothing was ever brought up. Fix the image; \
                 no amount of cable will help."
            } else if wired.is_empty() {
                "No wired adapter was found at all. Check that the USB Ethernet adapter \
                 is plugged in; `dmesg | grep r8152` shows whether the kernel bound a \
                 driver to it."
            } else if wired.iter().any(|i| i.up) {
                "The adapter is up but reports no carrier, which means the link did not \
                 negotiate. Check the cable and the port at the other end."
            } else {
                "A wired adapter was found but nothing could bring it up, so its carrier \
                 was never readable. Run `ip link set <name> up` by hand and see what it \
                 reports."
            };
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "waited {}s for a wired interface with a carrier; interfaces seen: \
                     {seen}. {remedy} Wireless can be joined from the console's Network \
                     view once the console is reachable, which is why setup still asks for \
                     a wired link: configuring the radio needs a network to configure it \
                     over.",
                    timeout.as_secs()
                ),
            ));
        }

        if !announced {
            announced = true;
            log("waiting for a wired link (USB Ethernet enumerates late)");
        }
        sleep(POLL);
    }
}

/// The loopback interface, which nothing else in this module will touch.
pub const LOOPBACK: &str = "lo";
/// Runs `ip` with the given arguments against the real machine.
///
/// Resolved through `PROGRAM_DIRS` like everything else here, because busybox installs
/// `ip` into `/sbin` and this process has no `PATH`. `ip` is silent when it succeeds, so
/// any output at all is it objecting and is returned as the error — a caller that only
/// checked the exit status would report success for a refused address.
///
/// # Errors
/// If `ip` is not in the image, cannot be run, or says anything.
pub fn ip(args: &[&str]) -> io::Result<()> {
    use plexos_gpu::env::{Environment as _, System};

    let program = resolve(&System, "ip").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("`ip` is in none of {}", PROGRAM_DIRS.join(", ")),
        )
    })?;

    match System.run(&program, args)? {
        output if output.trim().is_empty() => Ok(()),
        output => Err(io::Error::other(output.trim().to_owned())),
    }
}

/// Brings loopback up, because nothing else does and almost everything needs it.
///
/// `candidates` deliberately excludes loopback: it is not something to run DHCP on, and
/// it is never the answer to "what address do I type into a browser". The consequence
/// went unnoticed until Plex ran — it binds a listener on `127.0.0.1`, got
/// `EADDRNOTAVAIL`, and died with a C++ exception from inside Boost.ASIO that named
/// neither loopback nor an interface. The health gate's `plex-http` probe had the same
/// problem and reported it as Plex not answering.
///
/// Bringing it up is the whole fix: the kernel adds `127.0.0.1/8` itself on `NETDEV_UP`
/// for a device with `IFF_LOOPBACK` (`net/ipv4/devinet.c`, checked in this tree's kernel
/// rather than remembered), so there is no address to assign here and no second step to
/// get wrong.
///
/// Reported and never fatal. A machine whose loopback will not come up is broken in a
/// way this cannot fix, and refusing to serve the console would remove the one tool for
/// finding out why.
pub fn bring_up_loopback(env: &impl Environment, log: &mut dyn FnMut(&str)) {
    let Some(ip) = resolve(env, "ip") else {
        log(&format!(
            "cannot bring {LOOPBACK} up: `ip` is in none of {}. Anything that binds a \
             loopback address will fail with EADDRNOTAVAIL, including Plex.",
            PROGRAM_DIRS.join(", ")
        ));
        return;
    };

    match env.run(&ip, &["link", "set", LOOPBACK, "up"]) {
        // `ip link set ... up` is silent when it works, so any output is it objecting.
        Ok(output) if output.trim().is_empty() => log(&format!("{LOOPBACK} is up")),
        Ok(output) => log(&format!(
            "bringing {LOOPBACK} up: ip said {:?}. Plex binds a listener on 127.0.0.1 \
             and will fail to start without it.",
            output.trim()
        )),
        Err(error) => log(&format!(
            "could not run {ip} to bring {LOOPBACK} up: {error}. Plex binds a listener \
             on 127.0.0.1 and will fail to start without it."
        )),
    }
}

/// Runs against all of them rather than a chosen one, because choosing requires the
/// Brings up every wired interface that is not up already, so `carrier` becomes
/// readable.
///
/// Being up is a precondition for the carrier, and the carrier requires being up. Takes
/// the interfaces it was given
/// rather than enumerating again, so that the caller's view and this one cannot
/// disagree about what exists.
///
/// Skipping the ones already up matters because this is called on every pass of
/// [`wait_for_link`]: without the filter it would re-issue `ip` twice a second at the
/// only interface that is working.
fn link_up(
    env: &impl Environment,
    all: &[Interface],
    ip: &str,
    warned: &mut BTreeSet<String>,
    log: &mut dyn FnMut(&str),
) {
    for interface in candidates(all).into_iter().filter(|i| !i.up) {
        let complaint = match env.run(ip, &["link", "set", &interface.name, "up"]) {
            // `Environment::run` reports a non-zero exit as `Ok`, so the exit status is
            // not available here and the output is the only evidence there is. `ip link
            // set ... up` is silent when it works, so anything at all is the command
            // objecting -- and swallowing it is what left the real failure invisible
            // and the timeout blaming a cable that was plugged in.
            Ok(output) if !output.trim().is_empty() => {
                format!(
                    "bringing {} up: ip said {:?}",
                    interface.name,
                    output.trim()
                )
            }
            Ok(_) => continue,
            Err(error) => format!(
                "could not run {ip} to bring {} up: {error}. That path came from a search \
                 of {}, so if this says it does not exist the image is missing busybox's \
                 `ip` applet.",
                interface.name,
                PROGRAM_DIRS.join(", ")
            ),
        };
        // Once per interface. This runs on every pass of the wait, and a fault repeated
        // twice a second for thirty seconds buries the boot messages around it.
        if warned.insert(interface.name.clone()) {
            log(&complaint);
        }
    }
}

/// Brings the network up: wait for a carrier, bringing links up meanwhile, then DHCP.
///
/// Returns the interface that was configured. **A failure here is reported and
/// otherwise ignored by callers** — a media appliance with no cable is a machine with
/// a network problem, not one that should refuse to finish booting or roll back its
/// operating system.
///
/// The `udhcpc` that keeps the lease is a *grandchild*: `-b` makes it fork and the
/// process spawned here exits at once. Something has to collect that exit or it is a
/// zombie for the life of the daemon — see the thread below.
///
/// # Errors
/// Fails if no wired interface acquires a carrier within `timeout`, or if `udhcpc`
/// could not be launched.
pub fn configure(
    env: &impl Environment,
    timeout: Duration,
    log: &mut dyn FnMut(&str),
) -> io::Result<Interface> {
    // No bring-up pass here. wait_for_link does it on every iteration, which is the
    // only placement that works when the interface arrives during the wait.
    let interface = wait_for_link(env, timeout, log)?;
    dhcp(env, &interface.name, log)?;
    Ok(interface)
}

/// Runs a DHCP client on one interface, and leaves it running.
///
/// Split out of [`configure`] so that wireless can use it: everything below was learnt
/// from a machine, and a second copy of it would be a second place to learn it all again.
/// The caller decides *which* interface and *whether* it is ready; this decides nothing.
///
/// # Errors
/// Fails if `udhcpc` is not installed or cannot be started.
pub fn dhcp(env: &impl Environment, name: &str, log: &mut dyn FnMut(&str)) -> io::Result<()> {
    // Absolute, for the reason in PROGRAM_DIRS: this is spawned from a process with no
    // PATH, and udhcpc lives in /sbin, which the glibc fallback does not include.
    let udhcpc = resolve(env, "udhcpc").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "`udhcpc` is in none of {}. {name} has a carrier, so the link itself is \
                 fine; give it an address by hand with `ip addr add <a.b.c.d/nn> dev {name}` \
                 until the image carries a DHCP client.",
                PROGRAM_DIRS.join(", "),
            ),
        )
    })?;

    // -b: keep trying in the background rather than giving up if the server is slow.
    // -R: release the lease on exit, so a reboot does not burn a second address.
    // The script is Buildroot's, at /usr/share/udhcpc/default.script; without it
    // udhcpc would obtain a lease and configure nothing, which looks exactly like a
    // DHCP server that never answered.
    //
    // PATH is set for the child because that script calls `route` by bare name for the
    // default gateway. busybox sh happens to install a usable default when PATH is
    // unset, so this is belt and braces rather than a fix -- but the whole reason this
    // module now resolves absolute paths is that the same assumption was wrong once.
    let child = std::process::Command::new(&udhcpc)
        .args(["-i", name, "-b", "-R"])
        .env("PATH", PROGRAM_DIRS.join(":"))
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "could not start {udhcpc} on {name}: {error}. The link is up, so a \
                     static address can still be set by hand with `ip addr add`."
                ),
            )
        })?;

    // On a thread, and this is not tidiness. `-b` makes udhcpc fork, so the process
    // spawned above exits immediately and the resident client is its child -- reparented
    // to PID 1, which now collects it. Nothing collected the *direct* child, so every
    // plexosd left one zombie behind, found by looking at a real machine after PID 1
    // started reaping and one turned up that was not PID 1's to reap.
    //
    // Waiting here would be wrong, because the exit is only immediate when udhcpc
    // backgrounds itself, and a thread cannot make the caller late either way.
    //
    // A general reaper in this process would be worse than the leak: `waitpid(-1)`
    // collects *any* child, and plexosd runs curl, ip, losetup and sha256sum through
    // `Command::output()`, which waits for a specific pid and fails with ECHILD if
    // something else got there first. `Child::wait` waits for this one.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });

    log(&format!("{name} has a carrier; udhcpc is running"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::path::Path;

    use super::*;
    use plexos_gpu::env::Fixture;

    /// Builds the sysfs shape one interface has, without a backing device.
    fn virtual_iface(
        fixture: Fixture,
        name: &str,
        if_type: &str,
        carrier: &str,
        uevent: &str,
    ) -> Fixture {
        let base = format!("{SYS_CLASS_NET}/{name}");
        fixture
            .file(format!("{base}/type"), format!("{if_type}\n"))
            .file(format!("{base}/carrier"), format!("{carrier}\n"))
            .file(format!("{base}/operstate"), "up\n")
            .file(format!("{base}/address"), "00:11:22:33:44:55\n")
            .file(format!("{base}/uevent"), uevent.to_owned())
    }

    /// The same, plus the `device` symlink only real hardware has.
    fn iface(fixture: Fixture, name: &str, if_type: &str, carrier: &str, uevent: &str) -> Fixture {
        let base = format!("{SYS_CLASS_NET}/{name}");
        virtual_iface(fixture, name, if_type, carrier, uevent).link(
            format!("{base}/device"),
            "../../../devices/pci0000:00/0000:02:00.0",
        )
    }

    fn wired(fixture: Fixture, name: &str, carrier: &str) -> Fixture {
        iface(fixture, name, "1", carrier, &format!("INTERFACE={name}\n"))
    }

    /// Marks an interface administratively up, the way sysfs reports it: hex, prefixed.
    /// `0x1003` is `IFF_UP | IFF_BROADCAST | IFF_MULTICAST`.
    fn admin_up(fixture: Fixture, name: &str) -> Fixture {
        fixture.file(format!("{SYS_CLASS_NET}/{name}/flags"), "0x1003\n")
    }

    /// Puts busybox's `ip` where it actually lives, so [`resolve`] can find it.
    ///
    /// `/sbin`, not `/bin`: that is the whole point of resolving rather than trusting
    /// the environment, and a fixture that put it in `/bin` would test nothing.
    fn with_ip(fixture: Fixture) -> Fixture {
        fixture.file("/sbin/ip", "")
    }

    /// A machine where `ip addr show` reports nothing until DHCP has had a moment.
    ///
    /// The same shape of problem as [`LateUsbAdapter`] and equally invisible to a
    /// fixture: the answer has to change between calls, and an immutable one can only
    /// describe a lease that was always there.
    struct LateLease {
        /// Queries remaining before the address appears.
        arrives_in: Cell<u32>,
    }

    impl LateLease {
        fn new(arrives_in: u32) -> Self {
            Self {
                arrives_in: Cell::new(arrives_in),
            }
        }
    }

    impl Environment for LateLease {
        fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            if path == Path::new("/sbin") {
                return Ok(vec![PathBuf::from("/sbin/ip")]);
            }
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn read(&self, _path: &Path) -> io::Result<String> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn read_link(&self, _path: &Path) -> io::Result<PathBuf> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn run(&self, _program: &str, _args: &[&str]) -> io::Result<String> {
            let remaining = self.arrives_in.get();
            if remaining > 0 {
                self.arrives_in.set(remaining - 1);
                // What udhcpc's interface looks like before the lease lands: up, and
                // carrying nothing but a link-local v6 address, which `-4` hides.
                return Ok(String::new());
            }
            Ok("2: eth0    inet 192.168.2.42/24 scope global eth0\n".to_owned())
        }
    }

    /// A machine whose USB Ethernet adapter enumerates a few passes into the wait, and
    /// whose `carrier` — like the kernel's — cannot be read at all until something has
    /// brought the interface up.
    ///
    /// Both halves are what the reference laptop does, and neither can be expressed by
    /// [`Fixture`], which is immutable: it describes a machine where every interface is
    /// present and readable from the first pass. That is the one situation that never
    /// occurs here, and a suite built only on it passes against code that brings
    /// nothing up.
    struct LateUsbAdapter {
        /// Enumerations remaining before `eth0` exists.
        appears_in: Cell<u32>,
        /// Whether `ip link set eth0 up` has run.
        up: Cell<bool>,
        /// Every command line this was asked to run, for the tests to assert on.
        ran: RefCell<Vec<String>>,
    }

    impl LateUsbAdapter {
        fn new(appears_in: u32) -> Self {
            Self {
                appears_in: Cell::new(appears_in),
                up: Cell::new(false),
                ran: RefCell::new(Vec::new()),
            }
        }
    }

    impl Environment for LateUsbAdapter {
        fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            // busybox puts `ip` here and nowhere on the glibc fallback path.
            if path == Path::new("/sbin") {
                return Ok(vec![PathBuf::from("/sbin/ip")]);
            }
            if path != Path::new(SYS_CLASS_NET) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            let base = PathBuf::from(SYS_CLASS_NET);
            let remaining = self.appears_in.get();
            if remaining > 0 {
                self.appears_in.set(remaining - 1);
                return Ok(vec![base.join("lo")]);
            }
            Ok(vec![base.join("eth0"), base.join("lo")])
        }

        fn read(&self, path: &Path) -> io::Result<String> {
            let file = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let interface = path
                .parent()
                .and_then(std::path::Path::file_name)
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let up = self.up.get();
            match (interface, file) {
                ("lo", "type") => Ok("772\n".to_owned()),
                ("lo", _) => Ok(String::new()),
                ("eth0", "type") => Ok("1\n".to_owned()),
                ("eth0", "uevent") => Ok("INTERFACE=eth0\n".to_owned()),
                ("eth0", "address") => Ok("00:e0:4c:68:09:89\n".to_owned()),
                ("eth0", "operstate") => Ok(if up { "up\n" } else { "down\n" }.to_owned()),
                ("eth0", "flags") => Ok(if up { "0x1003\n" } else { "0x1002\n" }.to_owned()),
                // The kernel's actual behaviour, and the entire reason bring-up has to
                // come first: EINVAL while the interface is down, not "0".
                ("eth0", "carrier") => {
                    if up {
                        Ok("1\n".to_owned())
                    } else {
                        Err(io::Error::from(io::ErrorKind::InvalidInput))
                    }
                }
                _ => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        }

        fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
            // Only real hardware has this, which is what makes eth0 a candidate.
            if path == Path::new(SYS_CLASS_NET).join("eth0").join("device") {
                return Ok(PathBuf::from(
                    "../../../devices/pci0000:00/0000:00:14.0/usb1/1-1.3",
                ));
            }
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn run(&self, program: &str, args: &[&str]) -> io::Result<String> {
            self.ran
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            // Only the absolute path works, exactly as on the appliance: a bare "ip"
            // reaches execvp with no PATH and dies with ENOENT.
            if program == "/sbin/ip" && args.starts_with(&["link", "set"]) {
                self.up.set(true);
            }
            Ok(String::new())
        }
    }

    #[test]
    fn the_arp_constants_are_the_kernel_header_values() {
        // Pinned against include/uapi/linux/if_arp.h. These are not ours to choose:
        // if this fails, the kernel changed and the code is what has to follow.
        assert_eq!(ARPHRD_ETHER, 1, "ARPHRD_ETHER is 1 in if_arp.h");
        assert_eq!(ARPHRD_LOOPBACK, 772, "ARPHRD_LOOPBACK is 772 in if_arp.h");
    }

    #[test]
    fn the_iff_up_flag_is_the_kernel_header_value() {
        // include/uapi/linux/if.h, enum net_device_flags: IFF_UP = 1<<0. Same rule as
        // above -- if this fails, the constant here is what changes.
        assert_eq!(IFF_UP, 0x1, "IFF_UP is 1<<0 in if.h");
    }

    #[test]
    fn being_up_is_read_from_flags_rather_than_inferred_from_operstate() {
        // These are different questions. An interface nothing has brought up and one
        // that is up with no cable both read operstate "down"; only IFF_UP separates
        // them, and they take opposite remedies.
        let down = wired(Fixture::new(), "eth0", "0");
        assert!(
            !interfaces(&down).unwrap()[0].up,
            "no flags file at all counts as not up"
        );

        let up = admin_up(down, "eth0");
        assert!(interfaces(&up).unwrap()[0].up, "0x1003 has IFF_UP set");
    }

    #[test]
    fn wireless_is_recognised_by_devtype_not_by_name() {
        // eth-named wireless devices exist, and wlan-named wired ones can be created
        // with a rename. The kernel's own answer is the one that counts.
        assert_eq!(
            classify("1", "DEVTYPE=wlan\nINTERFACE=eth9\n"),
            Kind::Wireless
        );
        assert_eq!(classify("1", "INTERFACE=wlan0\n"), Kind::Wired);
    }

    #[test]
    fn loopback_and_ethernet_are_distinguished_by_type() {
        assert_eq!(classify("772", ""), Kind::Loopback);
        assert_eq!(classify("1", ""), Kind::Wired);
        assert_eq!(classify("65534", ""), Kind::Other);
    }

    #[test]
    fn a_down_interface_reports_no_carrier_rather_than_failing() {
        // The kernel returns EINVAL reading `carrier` on a down interface. Treating
        // that as an error would abort enumeration on a machine whose cable is simply
        // not up yet -- the normal state early in boot.
        let fixture = Fixture::new()
            .file(format!("{SYS_CLASS_NET}/eth0/type"), "1\n")
            .file(format!("{SYS_CLASS_NET}/eth0/uevent"), "INTERFACE=eth0\n");
        let found = interfaces(&fixture).unwrap();
        assert_eq!(found.len(), 1);
        assert!(!found[0].carrier);
        assert_eq!(found[0].operstate, "unknown");
    }

    #[test]
    fn a_bridge_never_wins_over_the_card_it_sits_on() {
        // Found on the build host, which runs containers: br-64a89730b1d1, docker0 and
        // two veths are all ARPHRD_ETHER with a carrier, and all sort before enp2s0.
        // Selecting by type alone runs DHCP on a bridge that goes nowhere.
        let fixture = wired(
            virtual_iface(
                virtual_iface(Fixture::new(), "br-64a8", "1", "1", "INTERFACE=br-64a8\n"),
                "docker0",
                "1",
                "1",
                "INTERFACE=docker0\n",
            ),
            "enp2s0",
            "1",
        );
        let all = interfaces(&fixture).unwrap();
        assert_eq!(
            preferred(&all).unwrap().name,
            "enp2s0",
            "only an interface with a backing device is real hardware"
        );
    }

    #[test]
    fn loopback_is_brought_up_even_though_it_is_never_a_candidate() {
        // The two facts have to coexist: loopback must never be chosen for DHCP, and it
        // must still be brought up. Holding only the first is what let Plex die binding
        // 127.0.0.1 with EADDRNOTAVAIL, from inside Boost.ASIO, with a message naming
        // neither loopback nor an interface.
        // Fixture::run answers from a table, so registering /sbin/ip with empty output
        // is how "the command ran and said nothing" is expressed -- which is what `ip
        // link set ... up` does when it works.
        let fixture = with_ip(Fixture::new()).command("/sbin/ip", "");
        let mut lines = Vec::new();
        bring_up_loopback(&fixture, &mut |line| lines.push(line.to_owned()));

        assert!(
            lines.iter().any(|l| l.contains("lo is up")),
            "loopback must be brought up: {lines:?}"
        );
    }

    #[test]
    fn a_missing_ip_command_says_what_will_break_rather_than_only_what_failed() {
        // Every diagnostic names a remedy, and the remedy here is not about loopback --
        // it is that anything binding a loopback address is about to fail in a way that
        // will not mention loopback at all.
        let fixture = Fixture::new();
        let mut lines = Vec::new();
        bring_up_loopback(&fixture, &mut |line| lines.push(line.to_owned()));

        let logged = lines.join("\n");
        assert!(logged.contains("EADDRNOTAVAIL"), "{logged}");
        assert!(logged.contains("Plex"), "{logged}");
    }

    #[test]
    fn loopback_is_never_a_candidate() {
        let fixture = virtual_iface(Fixture::new(), "lo", "772", "1", "");
        let all = interfaces(&fixture).unwrap();
        assert!(candidates(&all).is_empty(), "lo must never be configured");
        assert!(preferred(&all).is_none());
    }

    #[test]
    fn wireless_is_never_a_candidate_even_when_it_is_the_only_link() {
        // There is no wpa_supplicant in the image, so a wireless interface cannot be
        // associated. Running DHCP on it would wait out the whole timeout and then
        // report a network problem that was really a missing package.
        let fixture = iface(
            Fixture::new(),
            "wlan0",
            "1",
            "1",
            "DEVTYPE=wlan\nINTERFACE=wlan0\n",
        );
        let all = interfaces(&fixture).unwrap();
        assert!(preferred(&all).is_none());
    }

    #[test]
    fn a_wired_interface_with_a_carrier_wins_over_one_without() {
        let fixture = wired(wired(Fixture::new(), "eth0", "0"), "eth1", "1");
        let all = interfaces(&fixture).unwrap();
        assert_eq!(preferred(&all).unwrap().name, "eth1");
    }

    #[test]
    fn selection_is_stable_when_several_interfaces_have_carriers() {
        // Two adapters plugged in must not produce a different answer on each boot;
        // a status page that reported a different address every reboot would be worse
        // than one that reported none.
        let fixture = wired(wired(Fixture::new(), "eth1", "1"), "eth0", "1");
        let all = interfaces(&fixture).unwrap();
        assert_eq!(preferred(&all).unwrap().name, "eth0");
    }

    #[test]
    fn addresses_are_parsed_from_real_ip_output() {
        // Captured from `ip -o -4 addr show`, including the trailing backslash the
        // -o form emits, which a naive line split would leave attached to a field.
        let output = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n\
                      2: eth0    inet 192.168.2.42/24 brd 192.168.2.255 scope global dynamic eth0\\       valid_lft 42s preferred_lft 42s\n";
        let parsed = parse_addresses(output);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].interface, "eth0");
        assert_eq!(parsed[1].cidr, "192.168.2.42/24");
        assert_eq!(parsed[1].ip(), "192.168.2.42");
    }

    #[test]
    fn loopback_is_dropped_from_reported_addresses() {
        let fixture = with_ip(Fixture::new()).command(
            "/sbin/ip",
            "1: lo    inet 127.0.0.1/8 scope host lo\n\
             2: eth0    inet 192.168.2.42/24 scope global eth0\n",
        );
        let found = addresses(&fixture);
        assert_eq!(
            found.len(),
            1,
            "127.0.0.1 is never the answer to 'what URL'"
        );
        assert_eq!(found[0].ip(), "192.168.2.42");
    }

    #[test]
    fn an_interface_with_no_address_yields_nothing_rather_than_a_broken_entry() {
        let fixture =
            with_ip(Fixture::new()).command("/sbin/ip", "2: eth0    inet6 fe80::1/64 scope link\n");
        assert!(addresses(&fixture).is_empty());
    }

    #[test]
    fn a_timeout_with_the_adapter_up_blames_the_cable() {
        // Every diagnostic names a remedy, and this is the state where "check the
        // cable" is the right one: the interface is up, so the carrier is readable, and
        // it says there is no link.
        let fixture =
            with_ip(admin_up(wired(Fixture::new(), "eth0", "0"), "eth0")).command("/sbin/ip", "");
        let mut lines = Vec::new();
        let error = wait_for_link(&fixture, Duration::from_millis(200), &mut |m| {
            lines.push(m.to_owned());
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let message = error.to_string();
        assert!(
            message.contains("eth0 (up, no carrier)"),
            "names what it saw, in the terms that pick the remedy: {message}"
        );
        assert!(message.contains("cable"), "names a remedy: {message}");
        assert_eq!(lines.len(), 1, "the wait is announced exactly once");
    }

    #[test]
    fn a_timeout_with_the_adapter_down_blames_the_bring_up_and_not_the_cable() {
        // The state the reference laptop was actually in, and the one the first version
        // misdiagnosed. The adapter was plugged in and the cable was live; nothing had
        // brought the interface up, so its carrier was unreadable. Telling someone to
        // check a cable here sends them to inspect the one thing that was fine.
        // `ip` is present but every invocation fails, which is the state that leaves an
        // adapter found and never raised.
        let fixture = with_ip(wired(Fixture::new(), "eth0", "0"));
        let error = wait_for_link(&fixture, Duration::from_millis(200), &mut |_| {}).unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("eth0 (not up)"),
            "distinguishes down from cableless: {message}"
        );
        assert!(
            message.contains("ip link set"),
            "names the remedy that matches: {message}"
        );
        assert!(
            !message.contains("Check the cable"),
            "and not the one that does not: {message}"
        );
    }

    #[test]
    fn programs_are_found_in_sbin_where_busybox_puts_them() {
        // The bug this pins: plexosd is exec'd by PID 1 and has no PATH, so glibc's
        // execvp falls back to /bin:/usr/bin -- verified with `getconf PATH` -- while
        // busybox installs `ip` and `udhcpc` only into /sbin and /usr/sbin. Naming the
        // program alone works when a person types it at the shell and fails from the
        // daemon, which is why the first fix to this module boot-tested as ENOENT.
        let fixture = with_ip(Fixture::new());
        assert_eq!(resolve(&fixture, "ip").as_deref(), Some("/sbin/ip"));
        assert_eq!(
            resolve(&fixture, "udhcpc"),
            None,
            "and reports honestly when a program really is absent"
        );
    }

    #[test]
    fn a_missing_ip_is_named_as_the_cause_rather_than_blamed_on_hardware() {
        // No `ip` anywhere. The adapter and the cable are irrelevant here, and saying
        // otherwise sends someone to the wrong end of the machine.
        let fixture = wired(Fixture::new(), "eth0", "0");
        let mut lines = Vec::new();
        let error = wait_for_link(&fixture, Duration::from_millis(200), &mut |m| {
            lines.push(m.to_owned());
        })
        .unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("`ip` could not be found"),
            "names the real cause: {message}"
        );
        assert!(
            !message.contains("Check the cable") && !message.contains("plugged in"),
            "and does not send anyone to the hardware: {message}"
        );
        assert!(
            lines.iter().any(|l| l.contains("image fault")),
            "and says whose problem it is: {lines:?}"
        );
    }

    #[test]
    fn an_address_that_arrives_after_dhcp_starts_is_still_reported() {
        // udhcpc is spawned, not waited on, so at the moment it starts there is never a
        // lease. Looking once means looking too early, every time -- which is why the
        // console printed no URL on a machine whose network was working.
        let env = LateLease::new(2);
        let found = wait_for_address(&env, "eth0", Duration::from_secs(10), &mut |_| {})
            .expect("the lease arrives a moment later and must still be seen");
        assert_eq!(found.ip(), "192.168.2.42");
    }

    #[test]
    fn no_lease_within_the_timeout_is_reported_as_absence_rather_than_waited_on_forever() {
        // A machine with no DHCP server must still finish booting and serve its console.
        let env = LateLease::new(u32::MAX);
        let mut lines = Vec::new();
        let found = wait_for_address(&env, "eth0", Duration::from_millis(200), &mut |m| {
            lines.push(m.to_owned());
        });
        assert!(found.is_none());
        assert_eq!(lines.len(), 1, "the wait is announced exactly once");
    }

    #[test]
    fn an_adapter_that_appears_during_the_wait_is_still_brought_up() {
        // The regression this module exists to not repeat. Bringing links up once
        // before the wait cannot work: the interface being waited for is precisely the
        // one that does not exist yet when that single pass runs. It then sits
        // administratively down for the whole timeout with an unreadable carrier, and
        // the wait expires against an adapter that was plugged in the entire time.
        //
        // Against the old code -- link_up before the loop, a read-only wait -- this
        // times out. No fixture-based test could have caught it, because a fixture has
        // every interface present and readable from the first pass.
        let env = LateUsbAdapter::new(2);
        let mut lines = Vec::new();
        let found = wait_for_link(&env, Duration::from_secs(10), &mut |m| {
            lines.push(m.to_owned());
        })
        .expect("the adapter appears during the wait and must still be configured");

        assert_eq!(found.name, "eth0");
        assert!(found.carrier, "and is chosen only once it has a carrier");
        assert!(
            env.ran
                .borrow()
                .iter()
                .any(|c| c == "/sbin/ip link set eth0 up"),
            "which takes bringing it up after it appeared, by absolute path: {:?}",
            env.ran.borrow()
        );
    }

    #[test]
    fn an_interface_already_up_is_not_told_to_come_up_again() {
        // link_up runs on every pass now, so the filter is what stops it launching `ip`
        // twice a second at an interface that is working.
        let env = LateUsbAdapter::new(0);
        env.up.set(true);

        let found = wait_for_link(&env, Duration::from_secs(5), &mut |_| {}).unwrap();

        assert_eq!(found.name, "eth0");
        assert!(
            env.ran.borrow().is_empty(),
            "nothing to do, so nothing run: {:?}",
            env.ran.borrow()
        );
    }

    #[test]
    fn a_link_that_is_already_up_is_returned_without_waiting_or_logging() {
        let fixture = wired(Fixture::new(), "eth0", "1");
        let start = Instant::now();
        let mut lines = Vec::new();
        let found =
            wait_for_link(&fixture, LINK_TIMEOUT, &mut |m| lines.push(m.to_owned())).unwrap();

        assert_eq!(found.name, "eth0");
        assert!(start.elapsed() < Duration::from_secs(1), "must not poll");
        assert!(lines.is_empty(), "no wait, so nothing to announce");
    }
}
