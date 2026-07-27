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
//! anything. [`configure`] does them in that order for that reason, and
//! [`Interface::carrier`] is `false` for both "no cable" and "not up yet", which is why
//! the wait polls rather than deciding once.
//!
//! # What is verified and what is not
//!
//! Discovery, classification, selection and address parsing are pure functions over
//! [`Environment`], and are tested against recorded sysfs. **Launching `ip` and
//! `udhcpc` is not covered by tests** — it is two process launches, and a test that
//! mocked them would only compare this module to itself. It has not yet run on the
//! reference laptop; delete this notice once it has.

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

/// How long to wait for a wired interface to appear and acquire a carrier.
///
/// Matches [`plexos_sys::device::DEVICE_TIMEOUT`] deliberately: it is the same USB
/// enumeration delay being waited out, measured on the same machine, where at 12.9 s
/// into boot only a hub had appeared.
pub const LINK_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between rescans while waiting for a link.
const POLL: Duration = Duration::from_millis(500);

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
    let Ok(output) = env.run("ip", &["-o", "-4", "addr", "show"]) else {
        return Vec::new();
    };
    parse_addresses(&output)
        .into_iter()
        .filter(|a| a.interface != "lo")
        .collect()
}

/// Waits for a wired interface to appear and report a carrier.
///
/// Polls, because both halves of the condition arrive late and independently: the
/// device appears when USB enumerates, and the carrier appears when the link
/// negotiates. Announces the wait exactly once, and only when there is actually a
/// wait, so a machine with the cable already up logs nothing.
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

    loop {
        let all = interfaces(env).unwrap_or_default();
        if let Some(found) = preferred(&all) {
            return Ok(found.clone());
        }

        if Instant::now() >= deadline {
            let seen = all
                .iter()
                .filter(|i| i.kind != Kind::Loopback)
                .map(|i| format!("{} ({})", i.name, i.operstate))
                .collect::<Vec<_>>()
                .join(", ");
            let seen = if seen.is_empty() {
                "none".to_owned()
            } else {
                seen
            };
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "waited {}s for a wired interface with a carrier; interfaces seen: \
                     {seen}. Check that the USB Ethernet adapter is plugged in and the \
                     cable is connected. Wireless is not configurable in this release.",
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

/// Brings every wired interface up, so that `carrier` becomes readable.
///
/// Runs against all of them rather than a chosen one, because choosing requires the
/// carrier, and the carrier requires being up. See the module documentation.
fn link_up(env: &impl Environment, log: &mut dyn FnMut(&str)) {
    let all = interfaces(env).unwrap_or_default();
    for interface in candidates(&all) {
        if let Err(error) = env.run("ip", &["link", "set", &interface.name, "up"]) {
            log(&format!(
                "could not bring {} up: {error}. If `ip` is missing from the image the \
                 network cannot be configured at all.",
                interface.name
            ));
        }
    }
}

/// Brings the network up: links up, wait for a carrier, then DHCP.
///
/// Returns the interface that was configured. **A failure here is reported and
/// otherwise ignored by callers** — a media appliance with no cable is a machine with
/// a network problem, not one that should refuse to finish booting or roll back its
/// operating system.
///
/// `udhcpc` is spawned rather than waited on: it stays resident to renew the lease, so
/// waiting for it to exit would mean waiting for the lease to expire.
///
/// # Errors
/// Fails if no wired interface acquires a carrier within `timeout`, or if `udhcpc`
/// could not be launched.
pub fn configure(
    env: &impl Environment,
    timeout: Duration,
    log: &mut dyn FnMut(&str),
) -> io::Result<Interface> {
    link_up(env, log);
    let interface = wait_for_link(env, timeout, log)?;

    // -b: keep trying in the background rather than giving up if the server is slow.
    // -R: release the lease on exit, so a reboot does not burn a second address.
    // The script is Buildroot's, at /usr/share/udhcpc/default.script; without it
    // udhcpc would obtain a lease and configure nothing, which looks exactly like a
    // DHCP server that never answered.
    std::process::Command::new("udhcpc")
        .args(["-i", &interface.name, "-b", "-R"])
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "could not start udhcpc on {}: {error}. The link is up, so a static \
                     address can still be set by hand with `ip addr add`.",
                    interface.name
                ),
            )
        })?;

    log(&format!(
        "{} has a carrier; udhcpc is running",
        interface.name
    ));
    Ok(interface)
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn the_arp_constants_are_the_kernel_header_values() {
        // Pinned against include/uapi/linux/if_arp.h. These are not ours to choose:
        // if this fails, the kernel changed and the code is what has to follow.
        assert_eq!(ARPHRD_ETHER, 1, "ARPHRD_ETHER is 1 in if_arp.h");
        assert_eq!(ARPHRD_LOOPBACK, 772, "ARPHRD_LOOPBACK is 772 in if_arp.h");
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
        let fixture = Fixture::new().command(
            "ip",
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
        let fixture = Fixture::new().command("ip", "2: eth0    inet6 fe80::1/64 scope link\n");
        assert!(addresses(&fixture).is_empty());
    }

    #[test]
    fn waiting_names_the_adapter_and_the_cable_when_it_times_out() {
        // Every diagnostic names a remedy. A timeout that said only "no network" would
        // reproduce the problem this project exists to fix.
        let fixture = wired(Fixture::new(), "eth0", "0");
        let mut lines = Vec::new();
        let error = wait_for_link(&fixture, Duration::from_millis(200), &mut |m| {
            lines.push(m.to_owned());
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let message = error.to_string();
        assert!(message.contains("eth0"), "names what it saw: {message}");
        assert!(message.contains("plugged in"), "names a remedy: {message}");
        assert_eq!(lines.len(), 1, "the wait is announced exactly once");
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
