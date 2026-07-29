//! Giving the appliance a fixed address, without being able to lose it.
//!
//! A media server is a thing other machines connect *to*, so its address wants to stay
//! still, and a DHCP reservation is not always the owner's to make. Until now the only
//! answer was a lease.
//!
//! # This is the one setting that can take away the console
//!
//! Every other thing the page changes is recoverable from the page. A wrong address is
//! not: the browser loses the machine at the instant the change works, and what is left
//! is an appliance with no keyboard on an address nobody knows. The remedy would be the
//! attached screen, which is what this whole console exists to stop needing.
//!
//! So it borrows ADR-0005's shape rather than inventing one. The change is applied, and
//! then **undone unless somebody confirms it from the new address** within
//! [`CONFIRM_WITHIN`]. Confirming is proof the console is still reachable, which is
//! exactly the thing in doubt, and it cannot be faked by the machine itself.
//!
//! # Why it reverts to DHCP rather than to the previous static address
//!
//! Because a revert has to work on a machine whose network is already wrong, and DHCP is
//! the configuration that needs nothing to be true beforehand. Restoring a previous
//! static address would be restoring a setting that was, until a moment ago, the one
//! nobody had a problem with — but if the machine had *arrived* by DHCP, there is no
//! previous static address to go back to, and the code that decides which case it is on
//! a half-configured interface is exactly the code least likely to be right.
//!
//! # What has run
//!
//! **Nothing on hardware.**

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use plexos_types::config::{Config, NetworkConfig};

/// How long a new address has to be confirmed before it is undone.
///
/// Long enough for a browser to notice the old address stopped answering, for somebody to
/// type the new one, and for the page to load. Short enough that an unattended mistake
/// does not outlast the person who made it.
pub const CONFIRM_WITHIN: Duration = Duration::from_secs(120);

/// A change that is applied and not yet confirmed.
#[derive(Debug)]
struct Pending {
    /// What the configuration was before, to be written back on a revert.
    previous: Config,
    /// When it stops being provisional.
    deadline: Instant,
    /// What the address was changed to, for reporting.
    applied: String,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

fn pending() -> MutexGuard<'static, Option<Pending>> {
    PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the console reports about an address change in flight.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Trial {
    /// The address now in force, provisionally.
    pub address: String,
    /// Seconds left to confirm before it is undone.
    pub seconds_left: u64,
}

/// The change awaiting confirmation, if there is one.
#[must_use]
pub fn in_flight() -> Option<Trial> {
    let held = pending();
    let entry = held.as_ref()?;
    Some(Trial {
        address: entry.applied.clone(),
        seconds_left: entry
            .deadline
            .saturating_duration_since(Instant::now())
            .as_secs(),
    })
}

/// Records that an address change is provisional, and arms the revert.
///
/// `previous` is the configuration to write back. The revert runs on a thread rather than
/// from the next request, unlike the terminal's idle expiry, and the difference is the
/// point: if the address is wrong there *are* no next requests.
pub fn arm(previous: Config, applied: &str, path: std::path::PathBuf) {
    *pending() = Some(Pending {
        previous,
        deadline: Instant::now() + CONFIRM_WITHIN,
        applied: applied.to_owned(),
    });

    std::thread::spawn(move || {
        std::thread::sleep(CONFIRM_WITHIN);

        let Some(entry) = pending().take() else {
            // Confirmed, or superseded. Either way this thread has nothing to undo.
            return;
        };

        println!(
            "plexosd: addressing: nobody confirmed the new address within {}s, so it is \
             being undone. The console was presumably unreachable at it.",
            CONFIRM_WITHIN.as_secs()
        );

        // Written back before it is applied, so a machine that loses power between the
        // two comes up on the configuration that worked rather than the one that did not.
        if let Err(error) = crate::settings::save(&entry.previous, &path) {
            println!("plexosd: addressing: could not restore the configuration: {error}");
        }

        let mut log = |line: &str| println!("plexosd: addressing: {line}");
        apply(&entry.previous.network, &mut log);
    });
}

/// Confirms the change in flight, so nothing is undone.
///
/// Returns whether there was one. A confirmation with nothing to confirm is not an error
/// — a browser that retried, or that arrived after the deadline, should be told plainly
/// rather than shown a failure.
#[must_use]
pub fn confirm() -> bool {
    pending().take().is_some()
}

/// Why an address was refused before anything was done with it.
///
/// Checked here rather than left to `ip` to reject, because `ip` reports a usage message
/// on a malformed argument and the page would show a manual page to somebody who typed
/// one digit too many.
///
/// # Errors
/// A description naming what is wrong and what a correct value looks like.
pub fn validate(network: &NetworkConfig) -> Result<(), String> {
    if !network.is_static() {
        return Ok(());
    }

    let (address, prefix) = network.address.split_once('/').ok_or_else(|| {
        format!(
            "{:?} has no prefix length. Remedy: write it as 192.168.2.50/24 — without \
             one there is no way to know which addresses are on this network, and the \
             machine would be unreachable from all of them.",
            network.address
        )
    })?;

    parse_ipv4(address).ok_or_else(|| {
        format!("{address:?} is not an IPv4 address. Remedy: four numbers, 0 to 255.")
    })?;

    match prefix.parse::<u8>() {
        Ok(bits) if (1..=32).contains(&bits) => {}
        _ => {
            return Err(format!(
                "{prefix:?} is not a prefix length. Remedy: 1 to 32; a home network is \
                 almost always 24."
            ));
        }
    }

    if !network.gateway.is_empty() && parse_ipv4(&network.gateway).is_none() {
        return Err(format!(
            "{:?} is not a gateway address. Remedy: the router's address on this \
             network, or leave it empty for a machine that only talks to its own LAN.",
            network.gateway
        ));
    }

    for nameserver in &network.nameservers {
        if parse_ipv4(nameserver).is_none() {
            return Err(format!(
                "{nameserver:?} is not a nameserver address. Remedy: an IPv4 address, \
                 not a hostname — nothing can resolve a name before DNS works."
            ));
        }
    }

    Ok(())
}

/// A dotted quad, or nothing.
fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut parts = text.split('.');

    for octet in &mut octets {
        // `u8::from_str` accepts "007" and rejects "+7" and " 7", which is the behaviour
        // wanted here; what it does not reject is an empty string, hence the check.
        let part = parts.next()?;
        if part.is_empty() {
            return None;
        }
        *octet = part.parse().ok()?;
    }

    parts.next().is_none().then_some(octets)
}

/// Flushes whatever is on the interface and obtains a DHCP lease, now.
///
/// This is the revert path, and it has to restore *reachability*, not merely the file.
/// The first version of it only wrote the configuration back and promised a lease at the
/// next restart — which was fine on a machine whose static address happened to work, and
/// useless on the one case the whole confirmation exists for: an address nobody can reach.
/// The file would say DHCP and the machine would still be at the wrong address until
/// somebody power-cycled it, which is the trip to the attached screen this was meant to
/// prevent. Found by letting a real revert happen and looking at the interface after.
///
/// `-n -q` makes udhcpc block until it has a lease and then exit, so an address exists by
/// the time this returns rather than at some point afterwards. `-t`/`-T` bound that: a
/// revert that hung waiting for a DHCP server would leave the machine with no address at
/// all, which is worse than the state it was reverting from.
///
/// The background client started at boot is left alone. It will renew on its own
/// schedule; a second, short-lived one that exits is untidy rather than harmful, and
/// killing the first would need a pid this module does not have.
fn take_a_lease(interface: &str, log: &mut dyn FnMut(&str)) -> crate::settings::Outcome {
    if let Err(error) = crate::net::ip(&["addr", "flush", "dev", interface]) {
        return crate::settings::Outcome::Failed {
            detail: format!(
                "could not clear {interface}: {error}. The address that was set is still \
                 in force, so the machine is wherever that put it."
            ),
        };
    }
    log(&format!("{interface} flushed"));

    let Some(udhcpc) = [
        "/sbin/udhcpc",
        "/usr/sbin/udhcpc",
        "/bin/udhcpc",
        "/usr/bin/udhcpc",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).exists()) else {
        return crate::settings::Outcome::Failed {
            detail: format!(
                "{interface} has no address and `udhcpc` is not in this image. Remedy: \
                 set one by hand from the terminal with `ip addr add`."
            ),
        };
    };

    match std::process::Command::new(udhcpc)
        .args(["-i", interface, "-n", "-q", "-t", "6", "-T", "3"])
        .env("PATH", "/sbin:/usr/sbin:/bin:/usr/bin")
        .status()
    {
        Ok(status) if status.success() => crate::settings::Outcome::Applied {
            detail: format!("{interface} took a DHCP lease"),
        },
        Ok(_) => crate::settings::Outcome::Failed {
            detail: format!(
                "no DHCP server answered on {interface} within about 18 seconds, so it \
                 now has no address at all. Remedy: this needs the attached screen — \
                 check the cable and the router."
            ),
        },
        Err(error) => crate::settings::Outcome::Failed {
            detail: format!("could not run udhcpc on {interface}: {error}"),
        },
    }
}

/// Puts a network configuration into force.
///
/// Reports rather than returns a hard error: half of this is best-effort by nature — an
/// interface that has no gateway is a working machine on a flat network, not a failure.
pub fn apply(network: &NetworkConfig, log: &mut dyn FnMut(&str)) -> crate::settings::Outcome {
    use plexos_gpu::env::System;

    let Some(interface) = crate::net::interfaces(&System)
        .ok()
        .and_then(|all| crate::net::preferred(&all).map(|i| i.name.clone()))
    else {
        return crate::settings::Outcome::Failed {
            detail: "there is no wired interface to configure. Remedy: on this hardware \
                     Ethernet arrives over USB, so check the adapter is plugged in."
                .to_owned(),
        };
    };

    if !network.is_static() {
        return take_a_lease(&interface, log);
    }

    if let Err(error) = validate(network) {
        return crate::settings::Outcome::Failed { detail: error };
    }

    let mut steps = Vec::new();

    // flush first: without it the old address stays alongside the new one, the machine
    // answers on both, and the symptom is a change that appears to have done nothing.
    steps.push(vec![
        "addr".to_owned(),
        "flush".to_owned(),
        "dev".to_owned(),
        interface.clone(),
    ]);
    steps.push(vec![
        "addr".to_owned(),
        "add".to_owned(),
        network.address.clone(),
        "dev".to_owned(),
        interface.clone(),
    ]);
    if !network.gateway.is_empty() {
        steps.push(vec![
            "route".to_owned(),
            "replace".to_owned(),
            "default".to_owned(),
            "via".to_owned(),
            network.gateway.clone(),
            "dev".to_owned(),
            interface.clone(),
        ]);
    }

    for step in &steps {
        let args: Vec<&str> = step.iter().map(String::as_str).collect();
        match crate::net::ip(&args) {
            Ok(()) => log(&format!("ip {}", args.join(" "))),
            Err(error) => {
                return crate::settings::Outcome::Failed {
                    detail: format!(
                        "`ip {}` failed: {error}. Remedy: the address may already be in \
                         use, or the interface may have gone away.",
                        args.join(" ")
                    ),
                };
            }
        }
    }

    if !network.nameservers.is_empty() {
        use std::fmt::Write as _;
        let mut contents = String::new();
        for nameserver in &network.nameservers {
            let _ = writeln!(contents, "nameserver {nameserver}");
        }
        // /etc/resolv.conf is a symlink into /run; writing through it is what udhcpc does
        // too, so the two do not fight over which file is real.
        match std::fs::write("/etc/resolv.conf", contents) {
            Ok(()) => log("resolv.conf written"),
            Err(error) => log(&format!(
                "could not write /etc/resolv.conf: {error}. The address is set and names \
                 will not resolve."
            )),
        }
    }

    crate::settings::Outcome::Applied {
        detail: format!("{interface} is now {}", network.address),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statically(address: &str, gateway: &str) -> NetworkConfig {
        NetworkConfig {
            mode: "static".to_owned(),
            address: address.to_owned(),
            gateway: gateway.to_owned(),
            nameservers: vec![],
        }
    }

    #[test]
    fn dhcp_needs_no_validation() {
        assert!(validate(&NetworkConfig::default()).is_ok());
    }

    #[test]
    fn an_address_without_a_prefix_is_refused_and_says_why_it_matters() {
        let error = validate(&statically("192.168.2.50", "")).expect_err("must refuse");
        assert!(error.contains("Remedy:"), "{error}");
        assert!(error.contains("192.168.2.50/24"), "{error}");
    }

    #[test]
    fn nonsense_is_refused_before_ip_can_print_a_manual_page() {
        for address in [
            "not-an-address/24",
            "192.168.2.50/0",
            "192.168.2.50/33",
            "192.168.2/24",
            "192.168.2.50.1/24",
            "192.168.2.300/24",
            "192.168..50/24",
        ] {
            assert!(
                validate(&statically(address, "")).is_err(),
                "{address:?} was accepted"
            );
        }
    }

    #[test]
    fn a_valid_address_passes_with_and_without_a_gateway() {
        assert!(validate(&statically("192.168.2.50/24", "")).is_ok());
        assert!(validate(&statically("192.168.2.50/24", "192.168.2.1")).is_ok());
        assert!(validate(&statically("192.168.2.50/24", "router")).is_err());
    }

    #[test]
    fn a_nameserver_must_be_an_address_because_nothing_can_resolve_a_name_yet() {
        let mut network = statically("192.168.2.50/24", "192.168.2.1");
        network.nameservers = vec!["dns.example.com".to_owned()];
        let error = validate(&network).expect_err("must refuse");
        assert!(error.contains("not a hostname"), "{error}");
    }

    #[test]
    fn confirming_nothing_is_not_an_error() {
        // A browser that retried, or arrived after the deadline, should be told plainly.
        assert!(!confirm());
        assert!(in_flight().is_none());
    }

    #[test]
    fn the_confirmation_window_outlasts_a_person_typing_an_address() {
        // Somebody has to notice the old address stopped answering, type the new one and
        // wait for the page. Two minutes is the smallest number that is not a race.
        assert!(CONFIRM_WITHIN >= Duration::from_secs(60));
        assert!(CONFIRM_WITHIN <= Duration::from_secs(600));
    }
}
