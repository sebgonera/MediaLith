//! The one document the console and the API both render.
//!
//! Everything the status page shows is derived here, from [`Environment`], so the
//! whole page can be rendered against a recorded machine in a test. That is the same
//! reasoning `plexos-gpu` gives for its own boundary, and it applies with more force
//! here: this page's job is to describe a machine nobody can log into.
//!
//! # What this deliberately does not report
//!
//! **Whether this slot has been marked permanent.** Answering it means mounting the
//! ESP and reading the boot entries, and [`esp`](crate::esp) does exactly that while
//! the gate is clearing the counter. Mounting it again from an HTTP handler — on an
//! unauthenticated endpoint anyone on the LAN can hit — would race the one write in
//! the system that decides whether the machine rolls back. The slot and root hash come
//! from the kernel command line instead, which is free to read and cannot race
//! anything. When [`plexos-update`] exists and there is a supervisor holding the ESP
//! state in memory, this can report it from there.
//!
//! [`plexos-update`]: https://github.com/sebgonera/OS

use std::path::Path;

use plexos_gpu::env::Environment;
use plexos_gpu::report::Report;
use serde::Serialize;

use crate::health::{self, Health};
use crate::net;

/// Command line key naming the slot that booted. Matches `plexos-init`'s parser.
///
/// Public because the rollback record needs the same answer from the same source, and a
/// second copy of the string is how two readers of one command line drift apart.
pub const KEY_SLOT: &str = "plexos.slot";
/// Command line key carrying the dm-verity root hash.
const KEY_ROOTHASH: &str = "plexos.roothash";

/// What this image is and which half of the disk it booted from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Product {
    /// `PRETTY_NAME` from `/etc/os-release`.
    pub name: Option<String>,
    /// `VERSION_ID` from `/etc/os-release`.
    pub version: Option<String>,
    /// The `/usr` slot in use, `a` or `b`.
    pub slot: Option<String>,
    /// The dm-verity root hash `/usr` was verified against.
    pub root_hash: Option<String>,
    /// The whole kernel command line, verbatim.
    ///
    /// Reported because the root hash is not enough to tell two images apart. A change
    /// confined to the UKI — a kernel parameter, a console setting — leaves `/usr`
    /// byte-identical and therefore its hash unchanged, so a machine that was never
    /// reflashed and one that was look the same from here. That cost real time before
    /// this field existed: `i915.enable_guc=2` is invisible in every other field.
    ///
    /// It holds no secret. Everything on it is readable by anyone with a shell, and
    /// two of its values were already published above.
    pub cmdline: Option<String>,
    /// SHA-256 of the console's TLS public key, or `None` when it is not serving TLS.
    ///
    /// Here so the check is possible without the attached screen for anyone who has
    /// *already* reached the console once: compare it against what the browser shows.
    /// It is not a secret — it is a hash of a public key, and the browser shows it to
    /// anyone who connects — and it cannot stand in for the first comparison, which has
    /// nowhere to happen but the screen. ADR-0014 called that unresolved and it stays so.
    pub tls_fingerprint: Option<String>,
}

/// An interface as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InterfaceView {
    /// Kernel name.
    pub name: String,
    /// `wired`, `wireless`, `loopback` or `other`.
    pub kind: String,
    /// Whether a cable is detected.
    pub carrier: bool,
    /// The kernel's `operstate`.
    pub operstate: String,
    /// MAC address, if any.
    pub mac: Option<String>,
    /// Addresses configured on it, in CIDR form.
    pub addresses: Vec<String>,
}

/// The network as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkView {
    /// Every interface, loopback included, so an empty list reads as "sysfs is
    /// unreadable" rather than "no hardware".
    pub interfaces: Vec<InterfaceView>,
    /// The addresses someone could actually type into a browser.
    pub reachable_at: Vec<String>,
}

/// Everything the console renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// Image identity and slot.
    pub product: Product,
    /// The boot health gate's verdict, re-run now.
    pub health: Health,
    /// Whether the gate would pass, so the page does not have to re-implement the
    /// rule that an all-`NotApplicable` result is not healthy.
    pub healthy: bool,
    /// The hardware transcoding verdict — the question the project exists to answer.
    pub gpu: Report,
    /// Interfaces and addresses.
    pub network: NetworkView,
    /// Seconds since boot, from `/proc/uptime`.
    pub uptime_seconds: Option<u64>,
}

/// Pulls one `KEY=value` out of a kernel command line.
///
/// Values are not quoted on the command line MediaLith builds, so this does not attempt
/// to unquote them; doing so would invent a syntax the producer never emits.
#[must_use]
pub fn cmdline_value(cmdline: &str, key: &str) -> Option<String> {
    cmdline.split_whitespace().find_map(|token| {
        let (name, value) = token.split_once('=')?;
        (name == key).then(|| value.to_owned())
    })
}

/// Pulls one key out of `/etc/os-release` form, stripping the quotes it does use.
#[must_use]
pub fn os_release_value(contents: &str, key: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// The integer part of the first field of `/proc/uptime`.
#[must_use]
pub fn parse_uptime(contents: &str) -> Option<u64> {
    let seconds = contents.split_whitespace().next()?;
    let whole = seconds.split('.').next()?;
    whole.parse().ok()
}

impl Status {
    /// Gathers everything, from the machine described by `env`.
    ///
    /// Nothing here fails: a missing file becomes a `None` in the document rather than
    /// an error that blanks the whole page. A console that refuses to render because
    /// it could not read `/etc/os-release` would hide the GPU verdict that is the
    /// reason anyone opened it.
    #[must_use]
    pub fn gather(env: &impl Environment) -> Self {
        Self::gather_with(env, Report::generate(env))
    }

    /// The same, with the GPU report supplied rather than generated.
    ///
    /// [`Report::generate`] runs `vainfo`, which is a process. That is the right price for
    /// a page somebody has open, and the wrong price for something that asks every few
    /// seconds for the life of the machine: the appliance dashboard would have spent a few
    /// per cent of a core, permanently, re-answering a question whose answer changes across
    /// a reboot.
    ///
    /// So the expensive part is passed in and the cheap parts are read here. The
    /// alternative — a second assembly of this document in the dashboard — would be two
    /// answers to "is this machine healthy", and the screen and the console page
    /// disagreeing about that is worse than either of them being wrong alone.
    #[must_use]
    pub fn gather_with(env: &impl Environment, gpu: Report) -> Self {
        let cmdline = env.read(Path::new("/proc/cmdline")).unwrap_or_default();
        let os_release = env.read(Path::new("/etc/os-release")).unwrap_or_default();
        let uptime = env.read(Path::new("/proc/uptime")).unwrap_or_default();

        let health = health::run_all();
        let addresses = net::addresses(env);
        let interfaces = net::interfaces(env).unwrap_or_default();

        let interfaces = interfaces
            .into_iter()
            .map(|interface| InterfaceView {
                addresses: addresses
                    .iter()
                    .filter(|a| a.interface == interface.name)
                    .map(|a| a.cidr.clone())
                    .collect(),
                kind: match interface.kind {
                    net::Kind::Wired => "wired",
                    net::Kind::Wireless => "wireless",
                    net::Kind::Loopback => "loopback",
                    net::Kind::Other => "other",
                }
                .to_owned(),
                name: interface.name,
                carrier: interface.carrier,
                operstate: interface.operstate,
                mac: interface.mac,
            })
            .collect();

        Self {
            product: Product {
                name: os_release_value(&os_release, "PRETTY_NAME"),
                version: os_release_value(&os_release, "VERSION_ID"),
                slot: cmdline_value(&cmdline, KEY_SLOT),
                root_hash: cmdline_value(&cmdline, KEY_ROOTHASH),
                cmdline: (!cmdline.trim().is_empty()).then(|| cmdline.trim().to_owned()),
                tls_fingerprint: crate::tls::fingerprint(),
            },
            healthy: health.is_healthy(),
            health,
            gpu,
            network: NetworkView {
                interfaces,
                reachable_at: addresses.iter().map(|a| a.ip().to_owned()).collect(),
            },
            uptime_seconds: parse_uptime(&uptime),
        }
    }

    /// The document as JSON.
    ///
    /// # Errors
    /// Fails only if serialisation does, which for this shape means an allocation
    /// failure.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_gpu::env::Fixture;

    #[test]
    fn the_whole_command_line_is_reported_because_the_root_hash_cannot_tell_images_apart() {
        // A change confined to the UKI leaves /usr byte-identical, so its verity hash is
        // unchanged and two different images report the same one. That happened with
        // i915.enable_guc=2, and without this field there was no way to tell from the
        // network whether a machine had been reflashed.
        let fixture = Fixture::new().file(
            "/proc/cmdline",
            "plexos.slot=a plexos.roothash=abc i915.enable_guc=2\n",
        );
        let product = Status::gather(&fixture).product;
        assert_eq!(product.root_hash.as_deref(), Some("abc"));
        assert_eq!(
            product.cmdline.as_deref(),
            Some("plexos.slot=a plexos.roothash=abc i915.enable_guc=2")
        );
    }

    #[test]
    fn an_unreadable_command_line_is_absent_rather_than_an_empty_string() {
        let product = Status::gather(&Fixture::new()).product;
        assert_eq!(product.cmdline, None);
    }

    #[test]
    fn the_slot_and_root_hash_come_off_the_command_line() {
        let cmdline = "initrd=\\init ro plexos.slot=b plexos.roothash=abc123 console=tty0\n";
        assert_eq!(cmdline_value(cmdline, KEY_SLOT).unwrap(), "b");
        assert_eq!(cmdline_value(cmdline, KEY_ROOTHASH).unwrap(), "abc123");
        assert!(cmdline_value(cmdline, "plexos.nothing").is_none());
    }

    #[test]
    fn a_key_that_is_a_suffix_of_another_is_not_confused_for_it() {
        // `plexos.slot` must not match `plexos.slot_backup`, and a naive `contains`
        // would report the wrong half of the disk as the running one.
        let cmdline = "plexos.slot_backup=a plexos.slot=b";
        assert_eq!(cmdline_value(cmdline, KEY_SLOT).unwrap(), "b");
    }

    #[test]
    fn os_release_values_lose_their_quotes() {
        let contents = "NAME=\"MediaLith\"\nVERSION_ID=0.1.0\nPRETTY_NAME=\"MediaLith 0.1.0\"\n";
        assert_eq!(
            os_release_value(contents, "PRETTY_NAME").unwrap(),
            "MediaLith 0.1.0"
        );
        assert_eq!(os_release_value(contents, "VERSION_ID").unwrap(), "0.1.0");
    }

    #[test]
    fn uptime_is_truncated_to_whole_seconds() {
        assert_eq!(parse_uptime("1234.56 9876.54\n").unwrap(), 1234);
        assert!(parse_uptime("").is_none());
    }

    #[test]
    fn an_address_is_attributed_to_the_interface_that_holds_it() {
        let fixture = Fixture::new()
            .file("/sys/class/net/eth0/type", "1\n")
            .file("/sys/class/net/eth0/uevent", "INTERFACE=eth0\n")
            .file("/sys/class/net/eth0/carrier", "1\n")
            .file("/sys/class/net/eth0/operstate", "up\n")
            .file("/sys/class/net/lo/type", "772\n")
            .file("/sys/class/net/lo/uevent", "")
            // /sbin, and keyed by the absolute path: net::addresses resolves the
            // program rather than naming it, because this runs with no PATH.
            .file("/sbin/ip", "")
            .command(
                "/sbin/ip",
                "1: lo    inet 127.0.0.1/8 scope host lo\n\
                 2: eth0    inet 192.168.2.42/24 scope global eth0\n",
            );

        let status = Status::gather(&fixture);
        let eth0 = status
            .network
            .interfaces
            .iter()
            .find(|i| i.name == "eth0")
            .unwrap();
        assert_eq!(eth0.addresses, vec!["192.168.2.42/24"]);
        assert_eq!(eth0.kind, "wired");

        let lo = status
            .network
            .interfaces
            .iter()
            .find(|i| i.name == "lo")
            .unwrap();
        assert!(
            lo.addresses.is_empty(),
            "loopback is listed but its address is not offered as a URL"
        );
        assert_eq!(status.network.reachable_at, vec!["192.168.2.42"]);
    }

    #[test]
    fn a_machine_missing_every_file_still_produces_a_document() {
        // The page exists to be readable when things are broken. If gather() could
        // fail, the one situation it is needed in is the one where it would not render.
        let status = Status::gather(&Fixture::new());
        assert!(status.product.version.is_none());
        assert!(status.network.reachable_at.is_empty());
        assert!(status.to_json().is_ok());
    }

    #[test]
    fn the_json_carries_the_gpu_verdict_at_the_top_level() {
        // The console's whole purpose. If the field is ever renamed, the page stops
        // showing the answer and shows nothing in its place.
        let status = Status::gather(&Fixture::new());
        let json = status.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("gpu").is_some(), "{json}");
        assert!(parsed.get("healthy").is_some(), "{json}");
    }
}
