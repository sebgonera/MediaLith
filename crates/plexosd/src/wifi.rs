//! Wireless: what is in range, joining one, and remembering it.
//!
//! # Why this is not simply another interface
//!
//! `net` brings up every wired adapter and runs DHCP on the first with a carrier. That is
//! safe because a cable is a decision somebody already made — plugging it in *is* the
//! configuration. Wireless has no such act: an interface with no configuration has nothing
//! to associate with, and one with a configuration is joining a named network with a
//! secret. So `net::candidates` excludes `Kind::Wireless` deliberately, and everything
//! about wireless lives here and starts from something stored on `/var`.
//!
//! # The order that matters
//!
//! **An interface that is down cannot scan, and reports nothing rather than failing.** That
//! is the same shape as the `carrier` trap already recorded for wired links — sysfs returns
//! `EINVAL` on a down interface rather than "no cable" — and it was found the same way, on
//! a machine: `iw dev wlan0 scan` printed an empty list until `ip link set wlan0 up` had
//! been run, with nothing anywhere saying why. So [`scan`] brings the interface up first,
//! every time, and does not treat "already up" as anything.
//!
//! # What is stored, and what is not
//!
//! The passphrase is never written down. `wpa_passphrase` turns it into the 256-bit PSK
//! that the protocol actually uses, and only that is saved — which is why
//! `BR2_PACKAGE_WPA_SUPPLICANT_PASSPHRASE` is in the defconfig. It is not a secret worth
//! much less than the passphrase, since it is all an attacker needs to join this one
//! network, but it is not the string somebody probably also uses elsewhere.
//!
//! `wpa_passphrase` echoes the passphrase back as a comment on the line above the PSK.
//! [`parse_psk`] takes the uncommented one, which is the only reason that function exists.
//!
//! # What has run
//!
//! Parsing is tested against `tools/captures/iw-scan-wlan0.txt`, which came off the
//! reference laptop. The bring-up, the association and DHCP have **not** run on a machine
//! yet, and this notice stays until they have.

use std::io;
use std::path::Path;

use plexos_gpu::env::Environment;
use serde::{Deserialize, Serialize};

/// The remembered network. On `/var`, because `/usr` goes back on a rollback and the
/// network the machine is reached over must not go with it.
pub const CONFIG: &str = "/var/lib/plexos/wifi.json";

/// The generated supplicant configuration. On `/run`, not `/var`: it is derived from
/// [`CONFIG`] and regenerating it is cheaper than keeping two files that can disagree.
pub const SUPPLICANT_CONF: &str = "/run/plexos/wpa_supplicant.conf";

/// How a network is protected.
///
/// `Psk` covers WPA and WPA2, which take the same credential and the same configuration
/// here. `Sae` is WPA3, which takes the same passphrase from a person and a different
/// key exchange from the supplicant — worth telling apart because a network offering only
/// SAE cannot be joined by a supplicant built without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// No encryption at all.
    Open,
    /// WEP. Present for naming rather than for support: it is broken, and a network
    /// offering it is one to say so about.
    Wep,
    /// WPA or WPA2 with a pre-shared key.
    Psk,
    /// WPA3.
    Sae,
    /// 802.1X. Needs a certificate or an identity, neither of which this console asks
    /// for; named so the reason can be given rather than "could not join".
    Enterprise,
}

impl Security {
    /// Whether this console can join such a network with a passphrase alone.
    #[must_use]
    pub fn joinable(self) -> bool {
        matches!(self, Self::Open | Self::Psk | Self::Sae)
    }

    /// Why not, for the ones it cannot. Every diagnostic names a remedy.
    #[must_use]
    pub fn refusal(self) -> Option<&'static str> {
        match self {
            Self::Wep => Some(
                "WEP is broken and this appliance will not join it. Remedy: set the access \
                 point to WPA2 or WPA3, which every device made since 2006 supports.",
            ),
            Self::Enterprise => Some(
                "This network uses 802.1X, which needs an identity or a certificate rather \
                 than a passphrase. Remedy: join over a wired connection, or use a network \
                 with a pre-shared key.",
            ),
            _ => None,
        }
    }
}

/// One access point seen by a scan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Network {
    /// The name. Empty when the access point does not broadcast one.
    pub ssid: String,
    /// The access point's hardware address. What tells two radios sharing a name apart.
    pub bssid: String,
    /// Received signal, in dBm. Negative; closer to zero is stronger.
    pub signal_dbm: f32,
    /// The channel's centre frequency. 2.4 GHz reaches further; 5 GHz carries more.
    pub frequency_mhz: u32,
    /// What credential joining it would take.
    pub security: Security,
    /// No SSID in the beacon. Such a network can still be joined, by typing its name.
    pub hidden: bool,
}

impl Network {
    /// Signal as a share of usable range, 0 to 1.
    ///
    /// -30 dBm is as good as it gets in the same room; below -90 nothing works. Linear
    /// between, which is wrong as physics and right as a bar on a page — the alternative
    /// is showing a person a negative decibel figure and letting them work it out.
    #[must_use]
    pub fn strength(&self) -> f32 {
        ((self.signal_dbm + 90.0) / 60.0).clamp(0.0, 1.0)
    }

    /// Whether this is the 5 GHz band. Worth showing: two entries with one name and very
    /// different signals are otherwise inexplicable.
    #[must_use]
    pub fn is_5ghz(&self) -> bool {
        self.frequency_mhz >= 4900
    }
}

/// Parses `iw dev <interface> scan`.
///
/// The format is one `BSS <bssid>(on <interface>)` line followed by indented fields, and
/// the fields that matter are `SSID:`, `signal:`, `freq:` and the presence of an `RSN:` or
/// `WPA:` block. Everything else — and there is a great deal of it, capabilities and rates
/// and measurement pilots — is ignored.
///
/// Two details come from the capture rather than from memory. `freq:` carries a decimal
/// point on this version of `iw` and did not on older ones, so it is parsed as a float and
/// rounded. And a hidden network has **no `SSID:` line at all** rather than an empty one,
/// which is why `hidden` is derived from its absence.
#[must_use]
pub fn parse_scan(output: &str) -> Vec<Network> {
    let mut found = Vec::new();
    let mut block: Option<(String, Vec<String>)> = None;

    let flush = |block: Option<(String, Vec<String>)>, found: &mut Vec<Network>| {
        let Some((bssid, lines)) = block else { return };
        let field = |name: &str| -> Option<String> {
            lines.iter().find_map(|line| {
                line.trim()
                    .strip_prefix(name)
                    .map(|rest| rest.trim().to_owned())
            })
        };

        let Some(signal) =
            field("signal:").and_then(|value| value.split_whitespace().next()?.parse::<f32>().ok())
        else {
            // No signal means this was not a BSS block at all -- a shell prompt, or a
            // capture cut off partway. Silently dropping it is right: the alternative is a
            // network in the list with no name and no strength.
            return;
        };
        // Split on the point rather than parsing a float and casting: `2412` and
        // `2412.0` both have the integer part that is wanted, and a cast from f32 to u32
        // is a lint and a silent truncation waiting for a malformed line.
        let frequency = field("freq:")
            .and_then(|value| value.split('.').next()?.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let ssid = field("SSID:");

        found.push(Network {
            hidden: ssid.as_ref().is_none_or(String::is_empty),
            ssid: ssid.unwrap_or_default(),
            bssid,
            signal_dbm: signal,
            frequency_mhz: frequency,
            security: security_of(&lines),
        });
    };

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("BSS ") {
            flush(block.take(), &mut found);
            let bssid = rest.split('(').next().unwrap_or(rest).trim().to_owned();
            block = Some((bssid, Vec::new()));
        } else if let Some((_, lines)) = block.as_mut() {
            lines.push(line.to_owned());
        }
    }
    flush(block, &mut found);
    found
}

/// What protects a network, from the body of its scan block.
fn security_of(lines: &[String]) -> Security {
    let body = lines.join("\n");
    let rsn = body.contains("RSN:");
    let wpa = body.contains("WPA:");

    if body.contains("Authentication suites:") && body.contains("802.1X") {
        return Security::Enterprise;
    }
    // SAE and PSK together is a transition-mode network: joinable either way, and PSK is
    // what the older supplicant path uses, so it is reported as the more compatible of the
    // two rather than as the newer one.
    if body.contains("SAE") && !body.contains("PSK") {
        return Security::Sae;
    }
    if rsn || wpa {
        return Security::Psk;
    }
    // No RSN and no WPA, but the beacon claims privacy: that is WEP and nothing else.
    if body.contains("Privacy") {
        return Security::Wep;
    }
    Security::Open
}

/// One entry per name, keeping the strongest, and sorted strongest first.
///
/// A scan of a normal house returns the same network three or four times — once per band
/// and once per access point — and a list that shows all of them asks somebody to pick
/// between two identical names. Hidden networks are dropped here rather than merged: they
/// have no name to merge on, and one row saying "hidden network" per access point is
/// noise. Joining one is done by typing its name.
#[must_use]
pub fn best_per_ssid(mut found: Vec<Network>) -> Vec<Network> {
    found.sort_by(|a, b| {
        b.signal_dbm
            .partial_cmp(&a.signal_dbm)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen = std::collections::HashSet::new();
    found.retain(|n| !n.hidden && seen.insert(n.ssid.clone()));
    found
}

/// A remembered network. The passphrase is not part of it -- see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// The network's name, as it is broadcast or as it was typed.
    pub ssid: String,
    /// The 256-bit pre-shared key, as 64 hex characters. Empty for an open network.
    #[serde(default)]
    pub psk: String,
    /// Whether the network has to be probed for by name.
    #[serde(default)]
    pub hidden: bool,
}

/// Takes the PSK out of `wpa_passphrase` output.
///
/// It prints the passphrase back as a comment directly above the key:
///
/// ```text
/// network={
///     ssid="MyNetwork"
///     #psk="a passphrase here"
///     psk=4fa9683b7e074d7da8220aa0139a48189ffaf49622ab5b468b370b93ae2b5ba8
/// }
/// ```
///
/// Taking the first match for `psk=` therefore takes the passphrase — in clear, into the
/// file this whole arrangement exists to keep it out of. The comment is skipped by
/// trimming first and refusing anything starting with `#`.
#[must_use]
pub fn parse_psk(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let key = line.strip_prefix("psk=")?;
        let key = key.trim();
        // 64 hex characters and nothing else. `wpa_passphrase` reports its errors on
        // stdout as ordinary text, so a rejected passphrase would otherwise be stored as
        // if it were a key.
        (key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit())).then(|| key.to_owned())
    })
}

/// The supplicant configuration for a remembered network.
///
/// `scan_ssid=1` only for a hidden network: it makes the supplicant probe for the name
/// rather than wait to be told it, which is slower and, on a network that does broadcast,
/// unnecessary.
#[must_use]
pub fn supplicant_conf(saved: &Saved) -> String {
    use std::fmt::Write as _;

    let mut conf = String::from("ctrl_interface=/var/run/wpa_supplicant\nupdate_config=0\n\n");
    conf.push_str("network={\n");
    let _ = writeln!(conf, "\tssid=\"{}\"", escape(&saved.ssid));
    if saved.hidden {
        conf.push_str("\tscan_ssid=1\n");
    }
    if saved.psk.is_empty() {
        conf.push_str("\tkey_mgmt=NONE\n");
    } else {
        // Unquoted: quoted means "this is a passphrase, hash it", and bare means "this is
        // already the key". Storing the key and then quoting it would hash the hash.
        let _ = writeln!(conf, "\tpsk={}", saved.psk);
    }
    conf.push_str("}\n");
    conf
}

/// Escapes what a network name may legally contain and this file may not.
fn escape(ssid: &str) -> String {
    ssid.replace('\\', "\\\\").replace('"', "\\\"")
}

/// What `wpa_cli status` reports, as the key-value pairs it prints.
///
/// Parsed generically rather than field by field: the set of keys differs between
/// versions and between association states, and the two this cares about — `wpa_state` and
/// `ssid` — are stable. A body that is not key-value at all, which is what it prints when
/// there is no supplicant to talk to, yields nothing and reads as "not associated".
#[must_use]
pub fn parse_status(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once('=')?;
            (!key.contains(' ')).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Whether a status body says the supplicant has finished associating.
#[must_use]
pub fn associated(status: &[(String, String)]) -> bool {
    status
        .iter()
        .any(|(key, value)| key == "wpa_state" && value == "COMPLETED")
}

/// The name of the wireless interface, if the machine has one.
///
/// The first by name, which on every machine with one radio is the only one.
///
/// # Errors
/// Fails if sysfs cannot be read at all.
pub fn interface(env: &impl Environment) -> io::Result<Option<String>> {
    let all = crate::net::interfaces(env)?;
    Ok(all
        .into_iter()
        .filter(|i| i.kind == crate::net::Kind::Wireless && i.physical)
        .map(|i| i.name)
        .min())
}

/// Brings the interface up, so that a scan can mean something.
///
/// Idempotent and cheap, and called every time rather than only when the interface looks
/// down: `operstate` reads `down` both for "nothing brought it up" and "no carrier", and a
/// radio that has not associated has no carrier by definition. Asking would take longer
/// than doing.
///
/// # Errors
/// Fails if `ip` is not installed or the interface does not exist.
pub fn bring_up(name: &str) -> io::Result<()> {
    crate::net::ip(&["link", "set", name, "up"])
}

/// What is in range.
///
/// Brings the interface up first — see the module docs, and note that the failure this
/// avoids is an **empty list**, not an error. A scan on a down interface succeeds and
/// finds nothing, which reads as "there are no networks here" on a machine surrounded by
/// them. That was found by a person typing `ifconfig wlan0 up` and watching a list appear.
///
/// A scan taken within a moment of the interface coming up can be refused with
/// `Device or resource busy` while the radio is still settling, so it is tried again once.
/// Twice is enough: the second attempt is a second later, and a radio that is not ready by
/// then has something else wrong with it that another retry will not fix.
///
/// # Errors
/// Fails if the interface cannot be brought up, if `iw` is not installed, or if the scan
/// itself reports an error both times.
pub fn scan(env: &impl Environment, name: &str) -> io::Result<Vec<Network>> {
    bring_up(name)?;

    let mut last = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        let output = env.run("/usr/sbin/iw", &["dev", name, "scan"])?;
        let found = parse_scan(&output);
        if !found.is_empty() {
            return Ok(found);
        }
        last = output;
    }

    // An empty result is not an error -- a machine can genuinely be somewhere with no
    // networks -- but an empty result *with* a complaint in it is, and the complaint is
    // the only thing that says which.
    if last.contains("busy") || last.contains("Operation not") || last.contains("failed") {
        return Err(io::Error::other(format!(
            "the wireless scan failed: {}. Remedy: check that {name} is up with `ip link`, \
             and that nothing else holds the radio.",
            last.trim()
        )));
    }
    Ok(Vec::new())
}

/// Reads the remembered network, if there is one.
#[must_use]
pub fn saved() -> Option<Saved> {
    let text = std::fs::read_to_string(CONFIG).ok()?;
    serde_json::from_str(&text).ok()
}

/// Remembers a network, readable only by root.
///
/// # Errors
/// Fails if `/var` cannot be written.
pub fn save(saved: &Saved) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let path = Path::new(CONFIG);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(saved)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    std::fs::write(path, body)?;
    // After the write, not before: a file created 0644 and narrowed afterwards was
    // world-readable for the length of one write, and the key is the whole point.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Forgets it.
///
/// # Errors
/// Fails if the file exists and cannot be removed. A file that is not there is not an
/// error: forgetting a network the machine does not remember has already succeeded.
pub fn forget() -> io::Result<()> {
    match std::fs::remove_file(CONFIG) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `iw dev wlan0 scan` from the reference laptop. Five blocks of a real fifty-five,
    /// with only the names and addresses changed -- see the header in the file.
    const SCAN: &str = include_str!("../../../tools/captures/iw-scan-wlan0.txt");

    /// `wpa_passphrase MyNetwork 'a passphrase here'`, exactly as the appliance printed it.
    const PASSPHRASE_OUTPUT: &str = "network={\n\tssid=\"MyNetwork\"\n\t#psk=\"a passphrase here\"\n\tpsk=4fa9683b7e074d7da8220aa0139a48189ffaf49622ab5b468b370b93ae2b5ba8\n}\n";

    #[test]
    fn a_real_scan_yields_every_network_in_it() {
        let found = parse_scan(SCAN);
        assert_eq!(found.len(), 5, "five blocks, five networks: {found:#?}");
        assert!(
            found.iter().all(|n| n.signal_dbm < 0.0),
            "signal is in dBm and negative"
        );
    }

    #[test]
    fn the_frequency_survives_a_decimal_point() {
        // This `iw` prints `freq: 2412.0` and an older one prints `freq: 2412`. A parser
        // written against the older format takes neither, and would report every network
        // on 0 MHz -- which the page would then show as "5 GHz: no".
        let found = parse_scan(SCAN);
        let bands: Vec<u32> = found.iter().map(|n| n.frequency_mhz).collect();
        assert!(bands.contains(&2412), "2.4 GHz channel 1: {bands:?}");
        assert!(bands.contains(&5540), "and a 5 GHz one: {bands:?}");
        assert!(
            found.iter().any(Network::is_5ghz),
            "which is what tells the two bands apart"
        );
    }

    #[test]
    fn the_kinds_of_protection_are_told_apart() {
        let found = parse_scan(SCAN);
        let of = |name: &str| {
            found
                .iter()
                .find(|n| n.ssid == name)
                .unwrap_or_else(|| panic!("{name} is in the capture"))
                .security
        };
        assert_eq!(of("HomeNetwork"), Security::Psk, "RSN with PSK is WPA2");
        assert_eq!(of("ModernNetwork"), Security::Sae, "RSN with SAE is WPA3");
        assert_eq!(
            of("OpenGuest"),
            Security::Open,
            "no RSN, no WPA, no Privacy"
        );
    }

    #[test]
    fn a_network_with_no_name_is_hidden_rather_than_nameless() {
        // A hidden network has no SSID line at all, which is not the same as an empty one
        // -- and a parser that looked for an empty value would report no hidden networks
        // ever, on any machine, with nothing saying so.
        let found = parse_scan(SCAN);
        let hidden: Vec<&Network> = found.iter().filter(|n| n.hidden).collect();
        assert_eq!(hidden.len(), 1, "one of the five: {found:#?}");
        assert!(hidden[0].ssid.is_empty());
        assert!(
            !hidden[0].bssid.is_empty(),
            "it still has an address, which is how it was seen at all"
        );
    }

    #[test]
    fn one_row_per_name_and_the_strongest_first() {
        let found = best_per_ssid(parse_scan(SCAN));
        let names: Vec<&str> = found.iter().map(|n| n.ssid.as_str()).collect();
        assert!(
            !names.contains(&""),
            "a hidden network has no name to offer: {names:?}"
        );
        let signals: Vec<f32> = found.iter().map(|n| n.signal_dbm).collect();
        assert!(
            signals.windows(2).all(|pair| pair[0] >= pair[1]),
            "strongest first: {signals:?}"
        );
    }

    #[test]
    fn the_passphrase_is_not_what_gets_stored() {
        // `wpa_passphrase` prints the passphrase back as a comment on the line above the
        // key. Taking the first `psk=` takes the comment -- and writes the passphrase, in
        // clear, into the file this whole arrangement exists to keep it out of.
        let key = parse_psk(PASSPHRASE_OUTPUT).expect("the capture holds a key");
        assert_eq!(
            key, "4fa9683b7e074d7da8220aa0139a48189ffaf49622ab5b468b370b93ae2b5ba8",
            "the uncommented psk, not the commented passphrase"
        );
        assert!(
            !key.contains("passphrase"),
            "and specifically not the passphrase"
        );
    }

    #[test]
    fn something_that_is_not_a_key_is_not_stored_as_one() {
        // wpa_passphrase reports a passphrase that is too short on stdout, as prose, and
        // exits non-zero. A parser that took whatever followed `psk=` would save the prose.
        assert_eq!(parse_psk("Passphrase must be 8..63 characters\n"), None);
        assert_eq!(parse_psk("psk=tooshort\n"), None);
        assert_eq!(parse_psk("\tpsk=\"quoted passphrase\"\n"), None);
    }

    #[test]
    fn a_stored_key_is_not_offered_to_be_hashed_again() {
        // Quoted means "this is a passphrase, hash it"; bare means "this is the key". The
        // key is stored, so quoting it would hash the hash and the machine would fail to
        // associate with a correct credential.
        let conf = supplicant_conf(&Saved {
            ssid: "HomeNetwork".to_owned(),
            psk: "4fa9683b7e074d7da8220aa0139a48189ffaf49622ab5b468b370b93ae2b5ba8".to_owned(),
            hidden: false,
        });
        assert!(
            conf.contains("psk=4fa9683b7e074d7da8220aa0139a48189ffaf49622ab5b468b370b93ae2b5ba8")
        );
        assert!(!conf.contains("psk=\""), "not quoted: {conf}");
        assert!(
            !conf.contains("scan_ssid"),
            "only a hidden network is probed for"
        );
    }

    #[test]
    fn an_open_network_asks_for_no_key_at_all() {
        let conf = supplicant_conf(&Saved {
            ssid: "OpenGuest".to_owned(),
            psk: String::new(),
            hidden: false,
        });
        assert!(conf.contains("key_mgmt=NONE"), "{conf}");
        assert!(!conf.contains("psk="), "{conf}");
    }

    #[test]
    fn a_hidden_network_is_probed_for_by_name() {
        let conf = supplicant_conf(&Saved {
            ssid: "Invisible".to_owned(),
            psk: "0".repeat(64),
            hidden: true,
        });
        assert!(conf.contains("scan_ssid=1"), "{conf}");
    }

    #[test]
    fn a_name_with_a_quote_in_it_does_not_end_the_line() {
        let conf = supplicant_conf(&Saved {
            ssid: "it\"s \\ mine".to_owned(),
            psk: String::new(),
            hidden: false,
        });
        assert!(conf.contains(r#"ssid="it\"s \\ mine""#), "{conf}");
    }

    #[test]
    fn no_supplicant_reads_as_not_associated_rather_than_as_an_error() {
        // Captured from the appliance before anything was configured:
        //   Failed to connect to non-global ctrl_ifname: wlan0  error: No such file or directory
        // It is not key-value, so it yields nothing -- and nothing is the truth.
        let status = parse_status(
            "Failed to connect to non-global ctrl_ifname: wlan0  error: No such file or directory\n",
        );
        assert!(status.is_empty(), "{status:?}");
        assert!(!associated(&status));
    }

    #[test]
    fn an_association_in_progress_is_not_an_association() {
        let scanning =
            parse_status("bssid=00:00:00:00:00:00\nssid=HomeNetwork\nwpa_state=SCANNING\n");
        assert!(!associated(&scanning), "still looking");
        let done = parse_status("bssid=02:00:00:00:00:00\nssid=HomeNetwork\nwpa_state=COMPLETED\n");
        assert!(associated(&done));
    }

    #[test]
    fn a_network_this_console_cannot_join_says_why() {
        // "Could not join" about an 802.1X network sends somebody to check a passphrase
        // that was never the problem.
        assert!(!Security::Enterprise.joinable());
        assert!(Security::Enterprise.refusal().unwrap().contains("802.1X"));
        assert!(Security::Wep.refusal().unwrap().contains("WPA2"));
        for kind in [Security::Open, Security::Psk, Security::Sae] {
            assert!(kind.joinable());
            assert!(kind.refusal().is_none());
        }
    }

    #[test]
    fn strength_is_a_share_and_not_a_decibel() {
        let at = |dbm: f32| {
            Network {
                ssid: String::new(),
                bssid: String::new(),
                signal_dbm: dbm,
                frequency_mhz: 2412,
                security: Security::Open,
                hidden: false,
            }
            .strength()
        };
        assert!((at(-30.0) - 1.0).abs() < 0.01, "in the same room");
        assert!(at(-95.0).abs() < 0.01, "and below the floor, not negative");
        assert!(at(-60.0) > 0.4 && at(-60.0) < 0.6, "halfway is halfway");
    }
}
