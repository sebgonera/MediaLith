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
//! **The passphrase is what gets stored, for every network.** One line of configuration —
//! `key_mgmt=SAE WPA-PSK WPA-PSK-SHA256` with the passphrase quoted — joins WPA2, WPA3 and
//! transition mode, because the supplicant derives whichever credential the access point
//! turns out to want.
//!
//! It was not always so, and what it cost is the reason this section is here. WPA2 had its
//! passphrase hashed into the 256-bit PSK by `wpa_passphrase`, and only the key was kept:
//! a real privacy gain, of a very small size, since the key is all an attacker needs to
//! join this one network. **SAE cannot use it.** WPA3 derives its key inside the handshake
//! from the passphrase itself, so a configuration written from a stored key has no way into
//! such a network at all — every BSS answers `skip RSN IE - key mgmt mismatch` and the
//! supplicant never reaches authentication.
//!
//! Which credential to keep was therefore decided by what a *scan* said the network was,
//! and that reading is a snapshot: wrong once, and the network is unjoinable for good; and
//! a router switched from WPA2 to WPA3 turns a remembered network into one the appliance
//! can no longer reach, reporting it as a problem with the network. Being unable to join is
//! worse than storing a string.
//!
//! A machine that stored a hashed key under an earlier release keeps working:
//! [`supplicant_conf`] still reads `Saved::psk`. Nothing writes one any more.
//!
//! # What has run
//!
//! Parsing is tested against `tools/captures/iw-scan-wlan0.txt`, which came off the
//! reference laptop.
//!
//! **The bring-up, the association and DHCP have now run on the appliance.** A WPA2 network
//! was scanned for, joined from the console page, and carried the machine onto the LAN and
//! out to the web. That is the first time anything in this module reached a radio.
//!
//! What has **not** run is a successful SAE association: the WPA3 half is still only the
//! configuration above and the refusal a machine gave when offered the wrong kind of
//! credential. Joining one is what would close it.
//!
//! Moving *between* two networks is what the first session found, and it is the reason
//! [`release_supplicant`] exists — the second join failed outright, because asking the
//! first supplicant to stop is not the same as it having stopped.

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

/// Where the supplicant puts its control socket, and where `wpa_cli` is told to look.
///
/// **Not `/var/run`, which is what every `wpa_supplicant` example in the world says and
/// what both programs default to.** On an ordinary distribution `/var/run` is a symlink to
/// `/run`; here it does not exist at all, because the running root holds only what
/// `plan.rs` puts there and `/var` is a partition whose layout the installer made. The
/// supplicant then cannot create its socket, exits immediately, and the only symptom is
/// `wpa_cli` failing to connect — which reads as "not associated yet" and, twenty-five
/// seconds later, as a wrong passphrase.
///
/// Fourth thing in this project to assume a path that a normal system has and this one
/// does not, after `/dev/mapper`, the two `by-partlabel` lookups, and `/tmp`.
pub const SUPPLICANT_CTRL: &str = "/run/wpa_supplicant";

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
    /// The 256-bit pre-shared key, as 64 hex characters. Empty unless the network takes
    /// one -- see `passphrase`, and only ever one of the two is set.
    #[serde(default)]
    pub psk: String,
    /// The passphrase itself, for a network that will not take a precomputed key.
    ///
    /// **WPA3 is why this exists.** SAE derives its key inside the handshake, from the
    /// passphrase, so a 256-bit PSK — which is what WPA2 actually uses on the wire and
    /// what this stored instead — is not a credential it can use at all. The symptom is
    /// `skip RSN IE - key mgmt mismatch` in the supplicant's debug log and, everywhere
    /// else, a network that is in range and never associates.
    ///
    /// So the passphrase is kept only where nothing else will do, and the hashed key
    /// everywhere it still works.
    #[serde(default)]
    pub passphrase: String,
    /// Whether the network has to be probed for by name.
    #[serde(default)]
    pub hidden: bool,
}

/// The supplicant configuration for a remembered network.
///
/// `scan_ssid=1` only for a hidden network: it makes the supplicant probe for the name
/// rather than wait to be told it, which is slower and, on a network that does broadcast,
/// unnecessary.
#[must_use]
pub fn supplicant_conf(saved: &Saved) -> String {
    use std::fmt::Write as _;

    let mut conf = String::new();
    let _ = writeln!(conf, "ctrl_interface={SUPPLICANT_CTRL}");
    conf.push_str("update_config=0\n\n");
    conf.push_str("network={\n");
    let _ = writeln!(conf, "\tssid=\"{}\"", escape(&saved.ssid));
    if saved.hidden {
        conf.push_str("\tscan_ssid=1\n");
    }
    if !saved.passphrase.is_empty() {
        // WPA3, or a network whose protection is not known because its name was typed.
        //
        // `SAE WPA-PSK WPA-PSK-SHA256` rather than one of them: an access point may offer
        // SAE only, both (transition mode), or PSK with management frames required, and
        // this one line joins all three. `ieee80211w=1` says "capable of protected
        // management frames", which SAE requires and WPA2 ignores.
        //
        // Quoted, which means "this is a passphrase": SAE needs the passphrase itself and
        // cannot be given a precomputed key.
        conf.push_str("\tkey_mgmt=SAE WPA-PSK WPA-PSK-SHA256\n\tieee80211w=1\n");
        let _ = writeln!(conf, "\tpsk=\"{}\"", escape(&saved.passphrase));
    } else if saved.psk.is_empty() {
        conf.push_str("\tkey_mgmt=NONE\n");
    } else {
        // Unquoted: quoted means "this is a passphrase, hash it", and bare means "this is
        // already the key". Storing the key and then quoting it would hash the hash.
        conf.push_str("\tkey_mgmt=WPA-PSK WPA-PSK-SHA256\n");
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

/// The association this machine is in, as `iw dev <name> link` describes it.
///
/// Separate from [`Network`], which is a thing seen in a scan. This is the one the radio is
/// actually on, and the difference matters: a scan is a list of possibilities from some
/// seconds ago, and this is a measurement of now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Link {
    /// The network's name.
    pub ssid: String,
    /// The access point it is associated with. One network has several.
    pub bssid: String,
    /// What the radio is tuned to, which is how the band is known.
    pub frequency_mhz: u32,
    /// Received signal strength, in dBm. Negative, and closer to zero is stronger.
    pub signal_dbm: f32,
    /// The band, as a person says it. Always [`band_of`] the frequency beside it.
    ///
    /// Carried in the report rather than worked out by whatever draws it, so the boundaries
    /// live in exactly one place. A page that did this arithmetic would be a second
    /// definition of where 5 GHz ends, and the two would agree until somebody changed one.
    pub band: &'static str,
}

impl Link {
    /// The signal as a share of the usable range, for something that draws a bar.
    ///
    /// The same scale [`Network::strength`] uses, and deliberately so: the bar beside the
    /// network in a scan and the bar beside the one this machine is on have to mean the
    /// same thing, or comparing them teaches something false.
    #[must_use]
    pub fn strength(&self) -> f32 {
        ((self.signal_dbm + 90.0) / 60.0).clamp(0.0, 1.0)
    }
}

/// Which band a frequency is in, as a person says it.
///
/// Worth naming because it is the property of a connection somebody acts on: 2.4 GHz
/// reaches further and carries less. The boundary that matters is the one between 5 and 6
/// GHz — both are named in the same thousands, so rounding to the nearest whole GHz files
/// every 6 GHz network under 5 GHz. This appliance has already met a router broadcasting
/// one name across all three.
#[must_use]
pub fn band_of(frequency_mhz: u32) -> &'static str {
    match frequency_mhz {
        0..=2999 => "2.4 GHz",
        3000..=5924 => "5 GHz",
        _ => "6 GHz",
    }
}

/// Reads the association out of `iw dev <name> link`.
///
/// `None` when the interface is not associated, which `iw` reports as `Not connected.` and
/// is an ordinary state rather than a failure.
///
/// Tested against `tools/captures/iw-link-wlan0.txt`, taken off the appliance. Two things
/// in that capture are the reason it is a capture: `freq` carries a decimal point, and
/// `signal` carries its unit inside the value. Each line is trimmed before its prefix is
/// matched, because `iw` indents with a tab and no terminal this was read through preserved
/// one — a parser anchored on `\t` would pass its test and fail on the machine.
#[must_use]
pub fn parse_link(output: &str) -> Option<Link> {
    let bssid = output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Connected to ")?
            .split_whitespace()
            .next()
            .map(ToOwned::to_owned)
    })?;

    let field = |name: &str| -> Option<String> {
        output.lines().find_map(|line| {
            line.trim()
                .strip_prefix(name)
                .map(|rest| rest.trim().to_owned())
        })
    };

    let frequency_mhz = field("freq:")
        .and_then(|value| value.split('.').next()?.trim().parse::<u32>().ok())
        .unwrap_or(0);

    Some(Link {
        ssid: field("SSID:").unwrap_or_default(),
        bssid,
        band: band_of(frequency_mhz),
        // Split on the point for the same reason the scan parser does: `5540.0` is what the
        // program prints, and `parse::<u32>` on that is an error rather than 5540.
        frequency_mhz,
        // The unit is inside the value -- `-57 dBm` -- so the number is the first word and
        // taking the whole rest of the line yields nothing at all.
        signal_dbm: field("signal:")
            .and_then(|value| value.split_whitespace().next()?.parse::<f32>().ok())?,
    })
}

/// The association this machine is in, or `None` if it is in none.
///
/// Every failure is `None` rather than an error. This answers a question a status page
/// asks; a machine with no radio, no `iw`, or no association all mean the same thing to the
/// caller — there is nothing to draw — and turning any of them into an error would make a
/// page that reports a working appliance stop reporting anything.
#[must_use]
pub fn link(env: &impl Environment, name: &str) -> Option<Link> {
    let iw = program(env, "iw").ok()?;
    parse_link(&env.run(&iw, &["dev", name, "link"]).ok()?)
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
        let output = env.run(&program(env, "iw")?, &["dev", name, "scan"])?;
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

/// Where a join has got to.
///
/// A job rather than a synchronous request, and not for tidiness: `http::IO_TIMEOUT` is
/// fifteen seconds and an association is allowed twenty-five, so a request that did the
/// work would have its answer cut off precisely in the case worth reporting — the one
/// where the passphrase is wrong and the supplicant is still retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Nothing has been asked for.
    Idle,
    /// Looking for what is in range.
    Scanning,
    /// Handshaking with the access point.
    Associating,
    /// Associated; asking for an address.
    Addressing,
    /// On the network.
    Connected,
    /// It did not work, and `error` says what happened.
    Failed,
}

impl Phase {
    /// Whether a run holds the radio right now.
    #[must_use]
    pub fn is_running(self) -> bool {
        matches!(self, Self::Scanning | Self::Associating | Self::Addressing)
    }
}

/// The state of the one wireless job this daemon runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Progress {
    /// Where the run has got to, or `Idle` when there has not been one.
    pub phase: Phase,
    /// One line saying what is happening, in the words a person would use.
    pub detail: String,
    /// What went wrong, with a remedy. `None` unless the phase is `Failed`.
    pub error: Option<String>,
    /// What was in range at the last scan. Kept between requests so the page can show a
    /// list without holding the radio open on every poll.
    pub networks: Vec<Network>,
    /// A running commentary, bounded.
    pub log: Vec<String>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            detail: "nothing has been asked for".to_owned(),
            error: None,
            networks: Vec::new(),
            log: Vec::new(),
        }
    }
}

/// The one wireless job, and its progress.
#[derive(Debug, Default)]
pub struct Job {
    state: std::sync::Mutex<Progress>,
}

impl Job {
    /// A job that has never run.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current state.
    ///
    /// # Panics
    /// If a previous holder panicked. The state is a plain struct no operation can leave
    /// half-written, so taking it back is correct rather than merely convenient.
    #[must_use]
    pub fn snapshot(&self) -> Progress {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn with<R>(&self, f: impl FnOnce(&mut Progress) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state)
    }

    /// Claims the radio, if no other run holds it.
    ///
    /// Checked and set under one lock, so two requests arriving together cannot both be
    /// told to proceed — and two supplicants on one interface is a machine that
    /// associates and immediately disassociates, repeatedly, for no visible reason.
    pub fn begin(&self, phase: Phase, detail: &str) -> bool {
        self.with(|state| {
            if state.phase.is_running() {
                return false;
            }
            state.phase = phase;
            detail.clone_into(&mut state.detail);
            state.error = None;
            state.log = vec![detail.to_owned()];
            true
        })
    }

    /// Moves to a phase and records the line describing it.
    pub fn step(&self, phase: Phase, detail: &str) {
        self.with(|state| {
            state.phase = phase;
            detail.clone_into(&mut state.detail);
            if state.log.len() > 40 {
                state.log.remove(0);
            }
            state.log.push(detail.to_owned());
        });
    }

    /// Records what was found, without changing the phase.
    pub fn found(&self, networks: Vec<Network>) {
        self.with(|state| state.networks = networks);
    }

    /// Ends the run badly, keeping the reason.
    pub fn fail(&self, error: &str) {
        self.with(|state| {
            state.phase = Phase::Failed;
            "the network was not joined".clone_into(&mut state.detail);
            state.error = Some(error.to_owned());
            if state.log.len() > 40 {
                state.log.remove(0);
            }
            state.log.push(error.to_owned());
        });
    }
}

/// Finds one of the wireless programs, or says which is missing and what it does.
///
/// Through `net::resolve` rather than a literal path, for the reason recorded there: this
/// runs from a process with no `PATH`, and the glibc fallback covers `/bin:/usr/bin` while
/// these live in `/usr/sbin`.
fn program(env: &impl Environment, name: &str) -> io::Result<String> {
    crate::net::resolve(env, name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "`{name}` is not in this image, so wireless cannot be configured from here. \
                 Remedy: this is a build problem rather than a setting -- check \
                 BR2_PACKAGE_WPA_SUPPLICANT, its _CLI and _PASSPHRASE sub-options, and \
                 BR2_PACKAGE_IW in the defconfig."
            ),
        )
    })
}

/// Starts the supplicant in the foreground and drains everything it says.
///
/// **Not `-B`.** Backgrounding hands the process to init and closes the pipes with it, so
/// the supplicant's own account of why it would not start goes nowhere — and that is
/// exactly how a `ctrl_interface` naming a directory that does not exist here came to be
/// reported as a wrong passphrase, twenty-five seconds later. Kept in the foreground and
/// drained, so what it says is what gets reported.
///
/// Returns the child and the bounded buffer of what it has said.
///
/// # Errors
/// Fails if the program is missing or cannot be started.
fn start_supplicant(
    env: &impl Environment,
    name: &str,
) -> io::Result<(
    std::process::Child,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
)> {
    let supplicant = program(env, "wpa_supplicant")?;
    let mut child = std::process::Command::new(&supplicant)
        .args(["-i", name, "-c", SUPPLICANT_CONF, "-D", "nl80211"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "could not start {supplicant} on {name}: {error}. Remedy: the radio is \
                     present, so run `{supplicant} -i {name} -c {SUPPLICANT_CONF} -D \
                     nl80211` in the terminal to see what it says."
                ),
            )
        })?;

    let said = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    for stream in [
        child.stdout.take().map(drainable),
        child.stderr.take().map(drainable),
    ]
    .into_iter()
    .flatten()
    {
        let said = std::sync::Arc::clone(&said);
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::BufReader::new(stream)
                .lines()
                .map_while(Result::ok)
            {
                let mut held = said
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if held.len() > 40 {
                    held.remove(0);
                }
                held.push(line);
            }
        });
    }
    Ok((child, said))
}

/// How long to wait for a terminated supplicant to unlink its control socket.
///
/// It is an exit and a file removal, so it takes milliseconds. Five seconds is an
/// allowance for a machine under load, not an expectation.
const RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Stops whatever supplicant holds the radio, and does not return until it has let go.
///
/// `wpa_cli terminate` returns when the running supplicant *accepts* the command, not when
/// it has exited and unlinked its control socket. Starting the next one straight afterwards
/// is therefore a race, and it is one the new supplicant loses: it finds the socket still
/// bound, prints `ctrl_iface exists and seems to be in use - cannot override it`, and exits
/// 255. Reported off the appliance on the first attempt to move between two networks, and
/// the capture contains its own diagnosis — the old supplicant's `nl80211: deinit
/// ifname=wlan0` is the *last* line, printed after the new one had already given up.
///
/// A socket still there after the deadline is one of two things, and they take opposite
/// remedies. If something answers on it, the radio is genuinely held, and removing the file
/// would leave a live supplicant that no `wpa_cli` on the machine can reach — so that is
/// refused and named. If nothing answers, the file is a corpse: its owner died without
/// cleaning up, which is what SIGKILL does and what this module's own association timeout
/// used to send. Nothing else here would ever remove it, so one wrong passphrase left
/// wireless unjoinable until a reboot.
///
/// `patience` is [`RELEASE_TIMEOUT`] everywhere but the tests, which would otherwise spend
/// it twice over waiting for a deadline that is the whole point of two of them.
///
/// # Errors
/// Fails if a supplicant is still answering after `patience`, or if a socket nothing
/// answers on cannot be removed.
fn release_supplicant(
    env: &impl Environment,
    ctrl: &Path,
    cli: &str,
    name: &str,
    patience: std::time::Duration,
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    let socket = ctrl.join(name);
    // Nothing holds the radio, which is every first join of a boot.
    if !socket.exists() {
        return Ok(());
    }

    let dir = ctrl.to_string_lossy().into_owned();
    let _ = env.run(cli, &["-p", &dir, "-i", name, "terminate"]);

    let deadline = std::time::Instant::now() + patience;
    while socket.exists() {
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !socket.exists() {
        return Ok(());
    }

    // Still there, and whether anything is behind it is the whole question.
    let answered = env
        .run(cli, &["-p", &dir, "-i", name, "ping"])
        .is_ok_and(|reply| reply.contains("PONG"));
    if answered {
        return Err(io::Error::other(format!(
            "a wpa_supplicant is still holding {} {patience:?} after being asked to stop, \
             and it is still answering — so this is a running program and not a file left \
             behind. Remedy: find it with `ps` in the terminal and stop it there. Something \
             started a supplicant that this console did not, and removing the socket from \
             under it would leave it running and unreachable.",
            socket.display()
        )));
    }

    std::fs::remove_file(&socket).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{} is a control socket nothing answers on, and it could not be removed: \
                 {error}. Remedy: `rm {}` in the terminal. Until it is gone, every attempt \
                 to join a network exits 255 saying the interface is in use.",
                socket.display(),
                socket.display()
            ),
        )
    })?;
    log(&format!(
        "removed {}, left behind by a supplicant that did not exit cleanly; nothing was \
         answering on it",
        socket.display()
    ));
    Ok(())
}

/// How long to wait for the supplicant to associate before giving up.
///
/// Association is a handshake with an access point, and 25 seconds is long enough for a
/// slow one and short enough that a wrong passphrase is reported while somebody is still
/// looking at the page. A wrong key does not fail fast: the supplicant retries, so the
/// timeout *is* the error path.
const ASSOCIATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// Associates with a network and takes an address on it.
///
/// # Errors
/// Fails if the programs are missing, if the supplicant will not start, or if it has not
/// associated within [`ASSOCIATE_TIMEOUT`].
pub fn connect(
    env: &impl Environment,
    name: &str,
    network: &Saved,
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    bring_up(name)?;

    let conf = Path::new(SUPPLICANT_CONF);
    if let Some(parent) = conf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(conf, supplicant_conf(network))?;
    std::fs::set_permissions(conf, std::fs::Permissions::from_mode(0o600))?;

    // The socket directory, because the supplicant does not create it and exits when it
    // cannot bind. See SUPPLICANT_CTRL.
    std::fs::create_dir_all(SUPPLICANT_CTRL)?;

    // Whatever was running is for the previous network, and the next one cannot start until
    // it has actually let go. Asking is not the same as it having happened, which is the
    // whole of `release_supplicant`.
    let cli = program(env, "wpa_cli")?;
    release_supplicant(
        env,
        Path::new(SUPPLICANT_CTRL),
        &cli,
        name,
        RELEASE_TIMEOUT,
        log,
    )?;

    let (mut child, said) = start_supplicant(env, name)?;

    let quote = || {
        let held = said
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.join(" / ")
    };

    log(&format!("associating with {}", network.ssid));
    let deadline = std::time::Instant::now() + ASSOCIATE_TIMEOUT;
    let mut reached = String::from("nothing");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Before the status, because a supplicant that has already exited will never
        // produce one -- and waiting the full timeout to say so is how a missing directory
        // came to read as a wrong password.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(io::Error::other(format!(
                "wpa_supplicant stopped ({status}) instead of associating: {}. Remedy: run \
                 it in the terminal with the same arguments; whatever it says there is the \
                 whole answer.",
                quote()
            )));
        }

        let status = parse_status(
            &env.run(&cli, &["-p", SUPPLICANT_CTRL, "-i", name, "status"])
                .unwrap_or_default(),
        );
        if associated(&status) {
            break;
        }
        if let Some((_, value)) = status.iter().find(|(key, _)| key == "wpa_state") {
            reached.clone_from(value);
        }
        if std::time::Instant::now() >= deadline {
            // Taken **before** the supplicant is stopped, and that ordering is the point.
            // Stopping it makes it talk -- `CTRL-EVENT-DSCP-POLICY`, `deinit ifname=`,
            // `CTRL-EVENT-TERMINATING` -- and `said` is a forty-line ring, so our own
            // shutdown pushes out what it managed to say about the *network*. That is the
            // half somebody needs, and a report of a failed join that quotes only the
            // tidying-up describes this function rather than the radio. Cost a diagnosis on
            // the appliance: two joins failed for two entirely different reasons and the
            // second one's evidence had been overwritten by us.
            let said_about_the_network = quote();
            // `terminate` before a signal, and the difference is not politeness: a
            // supplicant killed outright never unlinks its control socket, so the next
            // attempt on this boot finds it bound and exits 255 — one wrong passphrase
            // disabling wireless until a reboot. The signal stays as the fallback for one
            // that will not go, and `release_supplicant` is what makes either survivable.
            let _ = env.run(&cli, &["-p", SUPPLICANT_CTRL, "-i", name, "terminate"]);
            std::thread::sleep(std::time::Duration::from_millis(500));
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            // Collected here rather than left for nobody: this path used to return without
            // waiting, which is a zombie per failed passphrase in a process that is not
            // PID 1 and does not reap.
            let _ = child.wait();
            // The state it reached is the difference between two failures that look
            // identical from outside. Stuck in SCANNING means the supplicant never found
            // a network it was willing to try -- wrong name, out of range, or a
            // credential of the wrong *kind*, which is what WPA3 does to a stored WPA2
            // key. Anything past that means it tried and was refused, which is a wrong
            // passphrase.
            let remedy = if reached == "SCANNING" {
                "Remedy: it never got as far as authenticating, so this is not the \
                 passphrase. Either the name is wrong, the network is out of range, or it \
                 wants a credential of a different kind -- scan again and join from the \
                 list, which tells the appliance what the network expects."
            } else {
                "Remedy: it found the network and was refused, which is what a wrong \
                 passphrase looks like -- the supplicant retries rather than refusing, so \
                 this timeout is how that is reported."
            };
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{} did not associate within {} seconds; it got as far as {reached}. \
                     The supplicant said: {}. {remedy}",
                    network.ssid,
                    ASSOCIATE_TIMEOUT.as_secs(),
                    said_about_the_network
                ),
            ));
        }
    }

    // Left running: it holds the association for as long as the machine is on it. Reaped
    // on a thread so that its eventual exit is collected rather than left as a zombie --
    // the shape that leaked one process per plexosd start when udhcpc was spawned with -b.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    log(&format!("associated with {}", network.ssid));
    crate::net::dhcp(env, name, log)
}

/// What `GET /api/wifi` answers.
///
/// The job alone is not enough, for the same reason provisioning's is not: after a reboot
/// the job is idle on a machine that is perfectly well connected, and a page reading only
/// the job would offer to join a network it is already on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// The wireless interface, or `None` on a machine with no radio.
    pub interface: Option<String>,
    /// The name of the remembered network. **Never the key** — this route needs no
    /// credential to read, and the key is on `/var` at 0600 for a reason.
    pub configured: Option<String>,
    /// Whether the supplicant has finished associating.
    pub connected: bool,
    /// The network it is on, which is not always the one that is remembered.
    pub ssid: Option<String>,
    /// The address held on the wireless interface, if any.
    pub address: Option<String>,
    /// The association itself: which access point, how strong, on what band.
    ///
    /// `None` on a machine with no radio and on one that is not associated. Separate from
    /// `ssid` above, which comes from the supplicant and says *what* it is on; this is the
    /// radio's own measurement of that connection, and it is the half a person reads to
    /// decide whether to move the appliance.
    pub link: Option<Link>,
    /// The run in flight, or the last one.
    #[serde(flatten)]
    pub progress: Progress,
}

/// The state of wireless on this machine, right now.
#[must_use]
pub fn report(env: &impl Environment, job: &Job) -> Report {
    let interface = interface(env).ok().flatten();
    let status = interface.as_ref().map_or_else(Vec::new, |name| {
        crate::net::resolve(env, "wpa_cli").map_or_else(Vec::new, |cli| {
            parse_status(
                &env.run(&cli, &["-p", SUPPLICANT_CTRL, "-i", name, "status"])
                    .unwrap_or_default(),
            )
        })
    });
    let field = |key: &str| {
        status
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value.clone())
    };
    let address = interface.as_ref().and_then(|name| {
        crate::net::addresses(env)
            .into_iter()
            .find(|a| &a.interface == name)
            .map(|a| a.cidr)
    });

    let link = interface.as_ref().and_then(|name| link(env, name));

    Report {
        connected: associated(&status),
        ssid: field("ssid"),
        address,
        configured: saved().map(|s| s.ssid),
        link,
        interface,
        progress: job.snapshot(),
    }
}

/// Scans, in the background, reporting into the job.
///
/// Backgrounded because a scan takes several seconds with the radio held open, which is
/// most of `http::IO_TIMEOUT` before anything else has happened.
pub fn spawn_scan(job: &std::sync::Arc<Job>) {
    let job = std::sync::Arc::clone(job);
    std::thread::spawn(move || {
        let env = plexos_gpu::env::System;
        let Ok(Some(name)) = interface(&env) else {
            job.fail(
                "this machine has no wireless interface. Remedy: if it has a card, the \
                 driver has not bound to it -- check `dmesg | grep -i iwlwifi` in the \
                 terminal, because a driver with no firmware registers nothing and looks \
                 exactly like a machine with no card.",
            );
            return;
        };
        match scan(&env, &name) {
            Ok(found) => {
                let count = found.len();
                job.found(best_per_ssid(found));
                job.step(Phase::Idle, &format!("{count} networks in range"));
            }
            Err(error) => job.fail(&error.to_string()),
        }
    });
}

/// Where the machine ended up after a join that did not work.
///
/// Three outcomes and they are not degrees of the same thing: one is ordinary, one is the
/// repair working, and one is an appliance that cannot be reached over the radio any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Landed<'a> {
    /// No network was remembered, so the radio held nothing before this and holds nothing
    /// now. Nothing was taken away.
    NowhereToGoBackTo,
    /// Back on the network it was on, which is the case this exists to produce.
    BackOn(&'a str),
    /// Not even that. The machine has no wireless network at all.
    Adrift {
        /// The network it could not get back onto.
        previous: &'a str,
        /// Why not.
        error: &'a str,
    },
}

/// What the job says after a join failed: why it failed, and where the machine is now.
///
/// Pure, and separate from the reconnecting, because the second half is the part somebody
/// acts on and it has to be right in all three cases. The orchestration around it is one
/// call to [`connect`].
///
/// The order is deliberate. Why the join failed is what was asked; where the machine ended
/// up is what has to be read *first* by somebody whose console just went quiet, so it is
/// the sentence the message ends on.
fn after_a_failed_join(wanted: &str, error: &str, landed: Landed) -> String {
    match landed {
        Landed::NowhereToGoBackTo => format!(
            "could not join {wanted}: {error} This machine was on no wireless network \
             before, so it is on none now — nothing was taken away by trying."
        ),
        Landed::BackOn(previous) => format!(
            "could not join {wanted}: {error} The machine is back on {previous}, which is \
             where it was before you asked."
        ),
        Landed::Adrift {
            previous,
            error: back,
        } => format!(
            "could not join {wanted}: {error} Going back to {previous} did not work \
             either: {back} This machine now has no wireless network at all. Remedy: join \
             one from the screen attached to it, which is the one way in that does not go \
             over the radio."
        ),
    }
}

/// Puts the machine back on the network it was on, after a join that did not work.
///
/// There is one radio and one supplicant, so joining a network means stopping the
/// association that is running before knowing whether the new one will work. A failed
/// attempt therefore used to leave the machine on *nothing* — and on an appliance reached
/// over that very interface, the console went away because somebody tried to move it to
/// another network. Reported from the machine on the first attempt to move from a WPA2
/// network to a WPA3 one, and the way back was the attached screen.
///
/// `save` happens only after a join works, so the remembered network is still the previous
/// one at every failure that reaches here.
fn go_back(
    env: &impl Environment,
    name: &str,
    previous: Option<&Saved>,
    wanted: &str,
    error: &io::Error,
    log: &mut dyn FnMut(&str),
) -> String {
    let error = format!("{error}.");
    let Some(previous) = previous else {
        return after_a_failed_join(wanted, &error, Landed::NowhereToGoBackTo);
    };

    log(&format!("going back to {}", previous.ssid));
    match connect(env, name, previous, log) {
        Ok(()) => after_a_failed_join(wanted, &error, Landed::BackOn(&previous.ssid)),
        Err(back) => after_a_failed_join(
            wanted,
            &error,
            Landed::Adrift {
                previous: &previous.ssid,
                error: &format!("{back}."),
            },
        ),
    }
}

/// Joins a network, in the background, and remembers it if it works.
///
/// Remembered **after** it associates, never before. A network recorded first is one the
/// machine tries again at every boot on the strength of a passphrase nobody has yet shown
/// to work — the same ordering rule as the anti-rollback sequence, for the same reason.
pub fn spawn_join(
    job: &std::sync::Arc<Job>,
    ssid: String,
    passphrase: String,
    hidden: bool,
    security: Option<Security>,
) {
    let job = std::sync::Arc::clone(job);
    std::thread::spawn(move || {
        let env = plexos_gpu::env::System;
        let Ok(Some(name)) = interface(&env) else {
            job.fail("this machine has no wireless interface.");
            return;
        };
        if let Some(refusal) = security.and_then(Security::refusal) {
            job.fail(refusal);
            return;
        }

        // The passphrase, always, and never a key computed from it.
        //
        // This used to branch on what the scan said: a network reported as WPA2 had its
        // passphrase hashed into a 256-bit PSK and only that was stored, which is a real
        // privacy gain of a very small size -- the key is all an attacker needs to join
        // this one network, and the code's own comment said so.
        //
        // What it cost is the whole of the connection. A stored PSK **cannot** be offered
        // to SAE, which derives its key inside the handshake from the passphrase itself, so
        // the configuration written from one has no way into a WPA3 network at all. That
        // makes a single wrong reading of a scan permanent, and it makes a router switched
        // from WPA2 to WPA3 into an appliance that can no longer reach its own network --
        // with a message about the network rather than about the credential.
        //
        // The passphrase joins WPA2, WPA3 and transition mode from one line, which is what
        // the config below writes. The rule the old branch applied only to a typed name is
        // the right rule everywhere: **being unable to join is worse than storing a
        // string.**
        //
        // A machine that stored a hashed key under an earlier release keeps working:
        // `supplicant_conf` still reads `Saved::psk`. It is read and never written.
        let network = if passphrase.is_empty() {
            Saved {
                ssid: ssid.clone(),
                psk: String::new(),
                passphrase: String::new(),
                hidden,
            }
        } else {
            Saved {
                ssid: ssid.clone(),
                psk: String::new(),
                passphrase: passphrase.clone(),
                hidden,
            }
        };

        // Read before anything replaces it. There is one radio, so joining stops the
        // association that is running before anybody knows the new one will work; without
        // this, a failed attempt leaves the machine on no network at all and takes the
        // console with it.
        let previous = saved();

        job.step(Phase::Associating, &format!("joining {ssid}"));
        let mut note = |line: &str| job.step(Phase::Addressing, line);
        if let Err(error) = connect(&env, &name, &network, &mut note) {
            let outcome = go_back(&env, &name, previous.as_ref(), &ssid, &error, &mut note);
            job.fail(&outcome);
            return;
        }
        if let Err(error) = save(&network) {
            job.fail(&format!(
                "joined {ssid}, but it could not be remembered: {error}. Remedy: the \
                 connection works now and will not survive a restart; check that /var is \
                 writable."
            ));
            return;
        }
        job.step(Phase::Connected, &format!("connected to {ssid}"));
    });
}

/// Lets stdout and stderr be drained by the same loop.
fn drainable(stream: impl std::io::Read + Send + 'static) -> Box<dyn std::io::Read + Send> {
    Box::new(stream)
}

/// Rejoins the remembered network at boot, in the background.
///
/// Nothing happens on a machine that has never been given one, which is every machine
/// until somebody joins a network from the console.
///
/// **In a thread, and this is not tidiness.** Association is allowed twenty-five seconds
/// and DHCP takes more; doing it before the console binds would delay the one thing a
/// person needs when the network is not working. The console comes up first and this
/// reports into the same job the page already polls, so a rejoin that fails is visible in
/// the Wireless card rather than only in a log.
///
/// It runs whether or not a cable has a carrier. Associating only when there is no cable
/// would mean a machine that had been given a network, and was plugged in, quietly did not
/// have it — which is what happened on the first appliance to be joined to one and
/// restarted, and reads as the setting not having been saved.
pub fn spawn_rejoin(job: &std::sync::Arc<Job>) {
    let Some(network) = saved() else {
        return;
    };
    if !job.begin(Phase::Associating, &format!("rejoining {}", network.ssid)) {
        return;
    }
    let job = std::sync::Arc::clone(job);
    std::thread::spawn(move || {
        let env = plexos_gpu::env::System;
        let Ok(Some(name)) = interface(&env) else {
            job.fail("this machine has no wireless interface any more.");
            return;
        };
        let outcome = {
            let mut note = |line: &str| job.step(Phase::Addressing, line);
            connect(&env, &name, &network, &mut note)
        };
        match outcome {
            Ok(()) => job.step(Phase::Connected, &format!("connected to {}", network.ssid)),
            Err(error) => job.fail(&error.to_string()),
        }
    });
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

    /// `iw dev wlan0 link` from the appliance, associated. See the header in the file.
    const LINK: &str = include_str!("../../../tools/captures/iw-link-wlan0.txt");

    #[test]
    fn the_association_is_read_out_of_what_iw_actually_prints() {
        let link = parse_link(LINK).expect("the capture is of an associated interface");
        assert_eq!(link.ssid, "HomeNetwork");
        assert_eq!(
            link.bssid, "32:70:4e:c9:b6:7c",
            "and not `(on wlan0)` with it"
        );
        assert!(
            (link.signal_dbm - -57.0).abs() < f32::EPSILON,
            "the unit is inside the value -- `-57 dBm` -- so the number is the first word \
             and the whole rest of the line parses as nothing: {}",
            link.signal_dbm
        );
        assert_eq!(
            link.frequency_mhz, 5540,
            "`freq` carries a decimal point here, exactly as it does in a scan"
        );
    }

    #[test]
    fn an_interface_on_no_network_is_not_an_error() {
        // What `iw` says when the radio is up and associated with nothing. It is the
        // ordinary state of an appliance somebody has not configured yet, and a report that
        // treated it as a failure would put an error on the page of a healthy machine.
        assert_eq!(parse_link("Not connected.\n"), None);
        assert_eq!(parse_link(""), None);
    }

    #[test]
    fn a_link_with_no_signal_line_is_no_link_rather_than_a_link_at_zero() {
        // Zero dBm is not "no reading" -- it is a stronger signal than any radio has ever
        // reported. Defaulting to it would draw a full bar for a measurement that was never
        // taken, which is the one direction this must not fail in.
        let truncated = "Connected to 32:70:4e:c9:b6:7c (on wlan0)\n\tSSID: HomeNetwork\n";
        assert_eq!(parse_link(truncated), None);
    }

    #[test]
    fn the_band_is_named_from_the_frequency_at_the_edges_that_matter() {
        let at = band_of;
        assert_eq!(at(2412), "2.4 GHz");
        assert_eq!(at(5540), "5 GHz", "the capture's own channel");
        // 5955 is the first 6 GHz channel and 5825 is among the last 5 GHz ones. The two
        // bands are named in the same thousands, so a boundary picked by rounding puts
        // 6 GHz networks in the 5 GHz row -- and this appliance has already met a router
        // broadcasting the same name on all three.
        assert_eq!(at(5825), "5 GHz");
        assert_eq!(at(5955), "6 GHz");
        assert_eq!(
            at(6175),
            "6 GHz",
            "the BSS this machine associated with by hand"
        );
    }

    #[test]
    fn a_link_and_a_scan_row_measure_strength_on_one_scale() {
        // Two bars drawn side by side that mean different things is worse than one bar.
        let scanned = Network {
            ssid: String::new(),
            bssid: String::new(),
            signal_dbm: -57.0,
            frequency_mhz: 5540,
            security: Security::Sae,
            hidden: false,
        };
        let associated = Link {
            ssid: String::new(),
            bssid: String::new(),
            frequency_mhz: 5540,
            signal_dbm: -57.0,
            band: band_of(5540),
        };
        assert!(
            (scanned.strength() - associated.strength()).abs() < f32::EPSILON,
            "{} vs {}",
            scanned.strength(),
            associated.strength()
        );
    }

    #[test]
    fn a_network_the_scan_called_wpa2_still_gets_a_credential_wpa3_could_use() {
        // The scan is a snapshot and the credential outlives it. This used to branch: a
        // network reported as `Psk` had its passphrase hashed and only the key stored,
        // which SAE cannot use at all -- so one wrong reading, or a router switched to
        // WPA3 afterwards, left an appliance that could not reach its own network and
        // said so as though the network were at fault.
        //
        // Asserted through `supplicant_conf` rather than by inspecting `Saved`, because
        // what matters is the line the supplicant is handed.
        let conf = supplicant_conf(&Saved {
            ssid: "HomeNetwork".to_owned(),
            psk: String::new(),
            passphrase: "a passphrase here".to_owned(),
            hidden: false,
        });
        assert!(
            conf.contains("key_mgmt=SAE WPA-PSK WPA-PSK-SHA256"),
            "one line has to cover WPA2, WPA3 and transition mode: {conf}"
        );
        assert!(
            conf.contains("psk=\"a passphrase here\""),
            "quoted, which is what tells the supplicant this is a passphrase and not a \
             precomputed key -- SAE can use the first and nothing at all with the second: \
             {conf}"
        );
    }

    #[test]
    fn a_stored_key_is_not_offered_to_be_hashed_again() {
        // Quoted means "this is a passphrase, hash it"; bare means "this is the key". The
        // key is stored, so quoting it would hash the hash and the machine would fail to
        // associate with a correct credential.
        let conf = supplicant_conf(&Saved {
            ssid: "HomeNetwork".to_owned(),
            psk: "4fa9683b7e074d7da8220aa0139a48189ffaf49622ab5b468b370b93ae2b5ba8".to_owned(),
            passphrase: String::new(),
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
    fn the_control_socket_goes_somewhere_that_exists_on_this_machine() {
        // `/var/run` is what every wpa_supplicant example says and what both programs
        // default to, because on an ordinary distribution it is a symlink to /run. Here it
        // does not exist: the running root holds only what plan.rs puts there. The
        // supplicant could not create its socket, exited at once, and `wpa_cli` then failed
        // to connect -- which reads as "not associated yet" and, twenty-five seconds later,
        // as a wrong passphrase. Every join failed that way, on every network.
        let conf = supplicant_conf(&Saved {
            ssid: "HomeNetwork".to_owned(),
            psk: "0".repeat(64),
            passphrase: String::new(),
            hidden: false,
        });
        assert!(
            !conf.contains("/var/run"),
            "/var/run does not exist on this appliance: {conf}"
        );
        assert!(
            conf.contains(&format!("ctrl_interface={SUPPLICANT_CTRL}")),
            "{conf}"
        );
        assert!(
            SUPPLICANT_CTRL.starts_with("/run/"),
            "and /run is one of the few directories the plan does create"
        );
    }

    #[test]
    fn a_wpa3_network_is_offered_the_passphrase_and_not_a_hashed_key() {
        // The first real network this met was WPA3, and every one of its access points
        // answered `skip RSN IE - key mgmt mismatch`: SAE derives its key inside the
        // handshake from the passphrase, so the 256-bit PSK that WPA2 uses on the wire is
        // not a credential it can take at all. The supplicant never got as far as
        // authenticating, and `wpa_state` stayed at SCANNING for the whole timeout.
        let conf = supplicant_conf(&Saved {
            ssid: "ModernNetwork".to_owned(),
            psk: String::new(),
            passphrase: "a passphrase here".to_owned(),
            hidden: false,
        });
        assert!(conf.contains("key_mgmt=SAE"), "{conf}");
        assert!(
            conf.contains("WPA-PSK"),
            "and the older kinds too, because an access point in transition mode offers \
             both and one line has to join either: {conf}"
        );
        assert!(
            conf.contains("ieee80211w=1"),
            "SAE requires protected management frames and WPA2 ignores the offer: {conf}"
        );
        assert!(
            conf.contains(r#"psk="a passphrase here""#),
            "quoted, which is what tells the supplicant this is a passphrase: {conf}"
        );
    }

    #[test]
    fn a_wpa2_network_still_never_sees_the_passphrase() {
        // The property is kept where it can be kept. Only WPA3 forces the passphrase to be
        // stored, and a network that takes a hashed key still gets one.
        let conf = supplicant_conf(&Saved {
            ssid: "HomeNetwork".to_owned(),
            psk: "4fa9683b7e074d7da8220aa0139a48189ffaf49622ab5b468b370b93ae2b5ba8".to_owned(),
            passphrase: String::new(),
            hidden: false,
        });
        assert!(
            conf.contains("psk=4fa9683b"),
            "bare, so it is used as the key rather than hashed again: {conf}"
        );
        assert!(!conf.contains("psk=\""), "{conf}");
        assert!(!conf.contains("SAE"), "{conf}");
    }

    #[test]
    fn an_open_network_asks_for_no_key_at_all() {
        let conf = supplicant_conf(&Saved {
            ssid: "OpenGuest".to_owned(),
            psk: String::new(),
            passphrase: String::new(),
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
            passphrase: String::new(),
            hidden: true,
        });
        assert!(conf.contains("scan_ssid=1"), "{conf}");
    }

    #[test]
    fn a_name_with_a_quote_in_it_does_not_end_the_line() {
        let conf = supplicant_conf(&Saved {
            ssid: "it\"s \\ mine".to_owned(),
            psk: String::new(),
            passphrase: String::new(),
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

    /// A machine whose `wpa_cli` tells `terminate` and `ping` apart, and remembers what it
    /// was asked.
    ///
    /// [`Fixture`] cannot express this. Its `run` discards the arguments and answers with
    /// one canned string per program, so `terminate` and `ping` are indistinguishable to
    /// it — and telling those two apart is the entire decision [`release_supplicant`]
    /// makes.
    struct Radio {
        /// Whether anything answers on the control socket.
        answers: bool,
        /// The socket a `terminate` removes, for a supplicant that exits when asked.
        released_by_terminate: Option<std::path::PathBuf>,
        /// Every command line, so a test can assert that stopping was tried before the
        /// socket was judged.
        ran: std::cell::RefCell<Vec<String>>,
    }

    impl Environment for Radio {
        fn list_dir(&self, _path: &Path) -> io::Result<Vec<std::path::PathBuf>> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn read(&self, _path: &Path) -> io::Result<String> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn read_link(&self, _path: &Path) -> io::Result<std::path::PathBuf> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn run(&self, program: &str, args: &[&str]) -> io::Result<String> {
            self.ran
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            if args.contains(&"terminate") {
                if let Some(socket) = &self.released_by_terminate {
                    let _ = std::fs::remove_file(socket);
                }
                return Ok("OK\n".to_owned());
            }
            if args.contains(&"ping") {
                return Ok(if self.answers {
                    "PONG\n".to_owned()
                } else {
                    String::new()
                });
            }
            Ok(String::new())
        }
    }

    #[test]
    fn a_failed_join_says_where_the_machine_ended_up_and_not_only_why() {
        let back = after_a_failed_join("GTFO", "it was refused.", Landed::BackOn("GTFO_Legacy"));
        assert!(
            back.contains("could not join GTFO"),
            "the question that was asked is still answered: {back}"
        );
        assert!(
            back.contains("back on GTFO_Legacy"),
            "and the half somebody acts on is where the machine is now: {back}"
        );
    }

    #[test]
    fn a_machine_left_with_no_network_says_so_and_names_the_way_back_in() {
        let adrift = after_a_failed_join(
            "GTFO",
            "it was refused.",
            Landed::Adrift {
                previous: "GTFO_Legacy",
                error: "the radio would not come up.",
            },
        );
        // This is the sentence that matters, because the console it would have been read
        // over is exactly what is gone. The only remaining way in is the attached screen,
        // and a message that did not say so would leave somebody refreshing a page for ever.
        assert!(
            adrift.contains("no wireless network at all"),
            "the state has to be stated plainly: {adrift}"
        );
        assert!(
            adrift.contains("screen attached"),
            "and every diagnostic here names a remedy: {adrift}"
        );
    }

    #[test]
    fn a_machine_that_was_on_nothing_is_not_told_it_lost_something() {
        let nothing = after_a_failed_join("GTFO", "it was refused.", Landed::NowhereToGoBackTo);
        assert!(
            nothing.contains("nothing was taken away"),
            "a first join that fails costs nothing, and saying otherwise would send \
             somebody looking for a connection that never existed: {nothing}"
        );
        assert!(
            !nothing.contains("screen attached"),
            "and it must not offer the emergency remedy, which is for the case where the \
             machine really did lose its way in: {nothing}"
        );
    }

    /// How long the tests let a socket stand before judging it. Two of them wait this out
    /// deliberately, and at the real five seconds that would be ten seconds of suite for
    /// nothing: what is under test is the decision at the far end, not the length of the
    /// wait.
    const BRIEF: std::time::Duration = std::time::Duration::from_millis(200);

    /// A control directory of this test's own, because Rust runs tests as threads in one
    /// process and a fixed path is one test deleting what another is reading.
    fn control_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("plexos-wifi-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[test]
    fn a_first_join_of_a_boot_has_nothing_to_release() {
        let dir = control_dir("nothing-to-release");
        let radio = Radio {
            answers: false,
            released_by_terminate: None,
            ran: std::cell::RefCell::new(Vec::new()),
        };

        release_supplicant(&radio, &dir, "wpa_cli", "wlan0", BRIEF, &mut |_| {})
            .expect("no socket is the ordinary case, not a failure");

        assert!(
            radio.ran.borrow().is_empty(),
            "nothing holds the radio, so nothing should have been asked to let go: {:?}",
            radio.ran.borrow()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_supplicant_that_takes_a_moment_to_go_is_waited_for_and_not_declared_dead() {
        let dir = control_dir("waits-for-release");
        let socket = dir.join("wlan0");
        std::fs::write(&socket, "").expect("a socket standing in for the previous run");

        // The defect this is about: `terminate` returns when the supplicant *accepts* it,
        // and the exit follows. So the socket outlives the call by a moment, which is
        // exactly the window the next supplicant used to start in and lose.
        let going = socket.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = std::fs::remove_file(&going);
        });

        let radio = Radio {
            answers: false,
            released_by_terminate: None,
            ran: std::cell::RefCell::new(Vec::new()),
        };

        let mut said = Vec::new();
        release_supplicant(
            &radio,
            &dir,
            "wpa_cli",
            "wlan0",
            std::time::Duration::from_secs(2),
            &mut |line| said.push(line.to_owned()),
        )
        .expect("a supplicant that goes when asked is the ordinary case");

        assert!(
            radio.ran.borrow().iter().any(|c| c.contains("terminate")),
            "the previous supplicant has to be asked to stop: {:?}",
            radio.ran.borrow()
        );
        assert!(
            !socket.exists(),
            "and the socket must be gone before the next supplicant starts, or it finds \
             the interface in use and exits 255"
        );

        // The two assertions that separate waiting from not waiting. A version that
        // returned straight after `terminate` would find the socket still there, ask
        // whether anything answered, get nothing, and unlink it -- so "the socket is gone"
        // is true of the broken code too. Only these say which way it got there, and
        // unlinking a socket whose supplicant is one millisecond from removing it himself
        // is how a live one ends up unreachable.
        assert!(
            !radio.ran.borrow().iter().any(|c| c.contains("ping")),
            "a supplicant that let go on its own was never a candidate for being a \
             leftover file, so nothing should have probed it: {:?}",
            radio.ran.borrow()
        );
        assert!(
            said.is_empty(),
            "and nothing was cleared away, so there is nothing to report: {said:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_socket_nothing_answers_on_is_removed_rather_than_blocking_every_later_join() {
        let dir = control_dir("stale-socket");
        let socket = dir.join("wlan0");
        std::fs::write(&socket, "").expect("a socket left behind by a killed supplicant");

        // Nothing removes it on `terminate`, because its owner is already dead -- which is
        // what SIGKILL leaves, and what the association timeout used to send.
        let radio = Radio {
            answers: false,
            released_by_terminate: None,
            ran: std::cell::RefCell::new(Vec::new()),
        };

        let mut said = Vec::new();
        release_supplicant(&radio, &dir, "wpa_cli", "wlan0", BRIEF, &mut |line| {
            said.push(line.to_owned());
        })
        .expect("a corpse is something to clear away, not something to fail on");

        assert!(
            !socket.exists(),
            "nothing else on the machine would ever remove it, so wireless would be \
             unjoinable until a reboot"
        );
        assert!(
            said.iter().any(|l| l.contains("did not exit cleanly")),
            "and removing a file out from under the system is worth a line: {said:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_supplicant_that_still_answers_is_not_unlinked_from_under_itself() {
        let dir = control_dir("live-socket");
        let socket = dir.join("wlan0");
        std::fs::write(&socket, "").expect("a socket with something behind it");

        // Asked to stop, does not stop, and still answers. Removing the file here would
        // leave a running supplicant that no wpa_cli on the machine can reach.
        let radio = Radio {
            answers: true,
            released_by_terminate: None,
            ran: std::cell::RefCell::new(Vec::new()),
        };

        let error = release_supplicant(&radio, &dir, "wpa_cli", "wlan0", BRIEF, &mut |_| {})
            .expect_err("a live supplicant that will not stop is a refusal");

        assert!(
            socket.exists(),
            "the socket belongs to something running and must survive"
        );
        let message = error.to_string();
        assert!(
            message.contains("still answering"),
            "the message has to separate this from a leftover file: {message}"
        );
        assert!(
            message.contains("ps"),
            "and every diagnostic here names a remedy: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
