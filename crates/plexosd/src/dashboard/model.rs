//! What the attached screen is saying, as a value (ADR-0019).
//!
//! Separated from the drawing for the reason the console page learned the hard way: a test
//! that asserts what is *in* a screen passes while the screen is wrong. Everything here is
//! a plain value computed from a machine, so "what does this appliance show when Plex is
//! down and the network is fine" is a question with an answer that does not need a
//! terminal, a framebuffer or a photograph.
//!
//! # Nothing here decides anything twice
//!
//! Every fact on this screen already had somewhere it was decided, and this reads it
//! there. The health verdict is [`crate::health`]'s, which is what ADR-0005's gate uses.
//! The address is [`crate::status`]'s `reachable_at`, which is what the browser is told to
//! type. The rollback line is [`crate::rollback::last_for`], which is the function that
//! exists because this project once announced a nine-day-old rollback in the future tense.
//!
//! A dashboard that worked any of those out for itself would be a second opinion, and the
//! two would drift — and the screen and the web console disagreeing about whether a machine
//! is healthy is worse than either of them being wrong on its own.

use std::time::Duration;

use plexos_gpu::env::Environment;

/// Whether the appliance can transcode on its GPU.
///
/// Four answers, and the fourth is the dashboard's own. `plexos-gpu` decides between
/// Ready, Degraded and Unavailable; `Unknown` is what this screen says before it has asked,
/// which on a booting machine is a real state and lasts a second or two.
///
/// `Degraded` is kept rather than folded into `Ready`, and that is the whole reason this
/// is not a boolean: the Alder Lake laptop transcoded on its GPU at reduced quality for
/// weeks because the initramfs carried firmware for one generation, and every layer above
/// reported success. A screen that said "Ready" about that machine would have been the
/// next layer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transcoding {
    /// The GPU can decode and encode, with nothing missing.
    Ready,
    /// It works, and something will make it slower or worse.
    Degraded,
    /// It cannot, and the report says why.
    Unavailable,
    /// Not asked yet.
    Unknown,
}

impl Transcoding {
    /// The word the screen shows.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Degraded => "Reduced",
            Self::Unavailable => "Unavailable",
            Self::Unknown => "Checking",
        }
    }

    /// What the GPU report says, reduced to the one thing a dashboard has room for.
    ///
    /// Read from [`plexos_gpu::report::Health`] rather than from the findings: the report
    /// already applies the rule that an unrecognised debugfs value is not a problem, and
    /// re-deriving it here is how the screen would come to disagree with `/api/gpu`.
    #[must_use]
    pub fn of(report: &plexos_gpu::report::Report) -> Self {
        match report.health {
            plexos_gpu::report::Health::Ready => Self::Ready,
            plexos_gpu::report::Health::Degraded => Self::Degraded,
            plexos_gpu::report::Health::Unavailable => Self::Unavailable,
        }
    }
}

/// Whether Plex is serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plex {
    /// Answering on loopback.
    Running,
    /// Installed and not answering.
    Stopped,
    /// Nothing is installed, which is the state of a machine nobody has set up yet.
    NotInstalled,
}

impl Plex {
    /// The word the screen shows.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Stopped => "Not responding",
            Self::NotInstalled => "Not installed",
        }
    }
}

/// The one line at the top: what is true of this machine, in order of what to say first.
///
/// Ordering is the whole of this type. Several of these can be true at once — a machine on
/// trial can also have no network — and a screen has one headline, so the question is which
/// fact a person walking up to the machine needs first.
///
/// Recovery leads because it is *news*: something happened while nobody was watching and
/// the machine is not running what it was. Then the trial, because it is about to become
/// news either way. Then the network, because without it nothing else on this screen can be
/// acted on remotely. Then Plex, then setup, then everything working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The previous release failed its health gate and this one replaced it.
    Recovered {
        /// The version that failed.
        failed: Option<String>,
    },
    /// This release is on trial: the bootloader is still counting tries.
    OnTrial {
        /// How many the bootloader has left.
        tries_left: u32,
    },
    /// No usable address, so nothing can reach the console.
    NoNetwork,
    /// The boot health gate is not satisfied.
    Unhealthy {
        /// What failed, in the gate's own words.
        failures: Vec<String>,
    },
    /// Plex is installed and not answering.
    PlexDown,
    /// Nothing is installed yet.
    NeedsSetup,
    /// Everything this screen knows how to check is fine.
    Working,
}

impl Verdict {
    /// The headline, and the mark in front of it.
    ///
    /// The mark is never the only signal — every one of these carries words as well —
    /// because the panel this is read on is a console with eight colours and a person
    /// standing two metres away.
    #[must_use]
    pub fn headline(&self) -> (Mark, String) {
        match self {
            Self::Recovered { failed } => (
                Mark::Warning,
                match failed {
                    Some(version) => format!("Recovered — {version} failed and was undone"),
                    None => "Recovered from a release that failed".to_owned(),
                },
            ),
            Self::OnTrial { tries_left } => (
                Mark::Testing,
                format!("Testing this release — {tries_left} boot(s) left to prove it"),
            ),
            Self::NoNetwork => (Mark::Warning, "No network address".to_owned()),
            Self::Unhealthy { failures } => (
                Mark::Warning,
                match failures.first() {
                    Some(first) => format!("Not healthy — {first}"),
                    None => "Not healthy".to_owned(),
                },
            ),
            Self::PlexDown => (Mark::Warning, "Plex is not responding".to_owned()),
            Self::NeedsSetup => (Mark::Testing, "Ready to set up".to_owned()),
            Self::Working => (Mark::Good, "Everything is working".to_owned()),
        }
    }
}

/// The symbol in front of the headline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Working.
    Good,
    /// Something needs attention.
    Warning,
    /// Neither: in progress, or waiting for somebody.
    Testing,
}

/// Everything the dashboard draws, gathered from the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// `PRETTY_NAME`, or the product name when there is none.
    pub product: String,
    /// `VERSION_ID`.
    pub version: Option<String>,
    /// The `/usr` slot in use.
    pub slot: Option<String>,
    /// Time since boot.
    pub uptime: Option<Duration>,
    /// Addresses a browser could be pointed at, best first.
    pub addresses: Vec<String>,
    /// The interface carrying the first of them.
    pub interface: Option<String>,
    /// The wireless interface, when this machine has one.
    ///
    /// Not part of a status report and deliberately not derived from one: a status report
    /// describes addresses and routes, and whether a radio is fitted is a fact about the
    /// hardware that is true whether or not anything is using it. [`Facts::gather`] fills
    /// this in; [`Facts::from_status`] leaves it `None`, because a status report cannot
    /// answer it and a field guessed from one would be a field that lies on the machines
    /// this feature exists for.
    pub wireless: Option<String>,
    /// Plex.
    pub plex: Plex,
    /// Hardware transcoding.
    pub transcoding: Transcoding,
    /// What to say at the top.
    pub verdict: Verdict,
}

impl Facts {
    /// Reads the machine.
    ///
    /// `gpu` is passed in rather than generated here because generating it runs `vainfo`,
    /// and this is called every few seconds for the life of the appliance. What the GPU can
    /// do does not change between two frames of a dashboard; it changes across a reboot.
    ///
    /// The *report* is passed too, for the same reason and more sharply: without it
    /// [`crate::status::Status::gather`] would generate one on every call, so a dashboard
    /// drawing a screen nobody is looking at would run `vainfo` twenty times a minute on a
    /// machine whose whole purpose is to have a spare core for transcoding.
    #[must_use]
    pub fn gather(
        env: &impl Environment,
        report: plexos_gpu::report::Report,
        gpu: Transcoding,
    ) -> Self {
        let mut status = crate::status::Status::gather_with(env, report);
        // Before the facts are derived, because the interface named beside the address is
        // looked up from whichever address comes first.
        prefer_covered(&mut status.network.reachable_at, crate::tls::covers);
        let mut facts = Self::from_status(&status, gpu, &crate::rollback::last_for);
        // Asked of the machine rather than of the status report, for the reason on the
        // field: a report about addresses cannot say whether a radio is fitted, and the
        // machines this matters for are exactly the ones with no address to report.
        facts.wireless = crate::wifi::interface(env).ok().flatten();
        facts
    }

    /// The same, from a gathered status — which is what makes every state below testable.
    ///
    /// `rollback` is a function rather than a value so that a test can describe a machine
    /// that has just been recovered without writing to `/var`.
    #[must_use]
    pub fn from_status(
        status: &crate::status::Status,
        gpu: Transcoding,
        rollback: &impl Fn(&str) -> Option<crate::rollback::Record>,
    ) -> Self {
        let plex = match status
            .health
            .checks
            .iter()
            .find(|check| check.name == crate::health::PLEX_HTTP)
        {
            Some(check) => match check.status {
                crate::health::Status::Pass => Plex::Running,
                crate::health::Status::Fail => Plex::Stopped,
                crate::health::Status::NotApplicable => Plex::NotInstalled,
            },
            None => Plex::NotInstalled,
        };

        let addresses = status.network.reachable_at.clone();
        let interface = status
            .network
            .interfaces
            .iter()
            .find(|interface| {
                addresses
                    .first()
                    .is_some_and(|first| interface.addresses.iter().any(|a| a.starts_with(first)))
            })
            .map(|interface| interface.name.clone());

        // The record only counts while it still describes the running system. That rule
        // lives in `rollback` and is called rather than repeated: this project has already
        // shipped a console announcing a rollback from nine days earlier, in the future
        // tense, on a healthy machine.
        let recovered = status.product.version.as_deref().and_then(rollback);

        let failures: Vec<String> = status
            .health
            .failures()
            .iter()
            .map(|check| format!("{}: {}", check.name, check.detail))
            .collect();

        // Plex failing *is* a health failure -- `plex-http` is one of the gate's checks --
        // so the two arms below would never both be reachable if this asked "is anything
        // failing" first. It asks "is anything **else** failing", and the difference is a
        // person at the machine reading "Plex is not responding" instead of
        // "Not healthy — plex-http: not answering", which is the gate's vocabulary and
        // nobody else's.
        let only_plex_failed = !failures.is_empty()
            && status
                .health
                .failures()
                .iter()
                .all(|check| check.name == crate::health::PLEX_HTTP);

        let verdict = if let Some(record) = recovered {
            Verdict::Recovered {
                failed: record.version,
            }
        } else if addresses.is_empty() {
            Verdict::NoNetwork
        } else if only_plex_failed {
            Verdict::PlexDown
        } else if !failures.is_empty() {
            Verdict::Unhealthy { failures }
        } else if plex == Plex::NotInstalled {
            Verdict::NeedsSetup
        } else {
            Verdict::Working
        };

        Self {
            product: status
                .product
                .name
                .clone()
                .unwrap_or_else(|| "MediaLith".to_owned()),
            version: status.product.version.clone(),
            slot: status.product.slot.clone(),
            uptime: status.uptime_seconds.map(Duration::from_secs),
            addresses,
            interface,
            // A status report cannot answer this, so it is not guessed from one. `gather`
            // asks the machine.
            wireless: None,
            plex,
            transcoding: gpu,
            verdict,
        }
    }

    /// The address a browser should be pointed at, if there is one.
    #[must_use]
    pub fn address(&self) -> Option<&str> {
        self.addresses.first().map(String::as_str)
    }
}

/// Moves the addresses this console's certificate names to the front.
///
/// A stable partition rather than a sort: the order `reachable_at` came in is the console
/// page's order and `/api/status`'s order, and this is not a second opinion about which
/// interface is primary. It only says that between two addresses that both reach this
/// machine, the one the certificate can vouch for is the one to put in front of somebody.
///
/// The reference laptop is why. It has a wired adapter and a wireless one, and the wireless
/// lease arrived *after* the certificate was issued -- so the screen said
/// `https://192.168.2.190` while the certificate named only `192.168.2.102`. Both reach the
/// console. What breaks is the fingerprint check at `/api/status`, which is the only thing
/// that makes a self-signed certificate mean anything, because the two would be about
/// different addresses.
///
/// Nothing is dropped. An appliance whose certificate names none of its addresses still
/// shows one, because an address that warns is worth more than no address at all.
pub fn prefer_covered(addresses: &mut Vec<String>, covered: impl Fn(&str) -> bool) {
    let (named, rest): (Vec<String>, Vec<String>) =
        addresses.iter().cloned().partition(|a| covered(a));
    *addresses = named.into_iter().chain(rest).collect();
}

/// A duration, in the coarsest unit that still says something.
///
/// Coarse on purpose, and the reason is the screen rather than taste. The dashboard writes
/// only when what it draws has changed, so that an untouched appliance stops writing and
/// the kernel's blank timer can turn the panel off — which is a thing this machine was
/// asked for by name. A seconds-resolution uptime would change every second, redraw every
/// second, and hold the panel lit all night.
///
/// It returns the value only. The caller says what it is: `humanUptime` on the console page
/// returned `"Up 1 h 21 min"` and both its callers added a label of their own, so the header
/// read **UP UP 18 MIN** on a real machine for a day.
#[must_use]
pub fn coarse(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if days > 0 {
        format!("{days} day{} {} h", plural(days), hours % 24)
    } else if hours > 0 {
        format!("{hours} h {} min", minutes % 60)
    } else if minutes > 0 {
        format!("{minutes} minute{}", plural(minutes))
    } else {
        "less than a minute".to_owned()
    }
}

fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{Check, Health, Status};
    use crate::status::{InterfaceView, NetworkView, Product};

    fn plex_check(status: Status) -> Check {
        Check {
            name: crate::health::PLEX_HTTP,
            status,
            detail: "for a test".to_owned(),
        }
    }

    fn a_machine(checks: Vec<Check>, addresses: &[&str]) -> crate::status::Status {
        let health = Health { checks };
        crate::status::Status {
            product: Product {
                name: Some("MediaLith 0.1.0.202608111733".to_owned()),
                version: Some("0.1.0.202608111733".to_owned()),
                slot: Some("b".to_owned()),
                root_hash: None,
                cmdline: None,
                tls_fingerprint: None,
            },
            healthy: health.is_healthy(),
            health,
            gpu: plexos_gpu::report::Report::generate(&plexos_gpu::env::Fixture::new()),
            network: NetworkView {
                interfaces: vec![InterfaceView {
                    name: "eth0".to_owned(),
                    kind: "wired".to_owned(),
                    carrier: true,
                    operstate: "up".to_owned(),
                    mac: None,
                    addresses: addresses.iter().map(|a| format!("{a}/24")).collect(),
                }],
                reachable_at: addresses.iter().map(|&a| a.to_owned()).collect(),
            },
            uptime_seconds: Some(125),
        }
    }

    fn no_rollback(_: &str) -> Option<crate::rollback::Record> {
        None
    }

    fn facts_for(checks: Vec<Check>, addresses: &[&str]) -> Facts {
        Facts::from_status(
            &a_machine(checks, addresses),
            Transcoding::Ready,
            &no_rollback,
        )
    }

    #[test]
    fn a_healthy_machine_says_everything_is_working() {
        let facts = facts_for(vec![plex_check(Status::Pass)], &["192.168.2.102"]);
        assert_eq!(facts.verdict, Verdict::Working);
        assert_eq!(facts.plex, Plex::Running);
        assert_eq!(facts.address(), Some("192.168.2.102"));
        assert_eq!(facts.interface.as_deref(), Some("eth0"));

        let (mark, said) = facts.verdict.headline();
        assert_eq!(mark, Mark::Good);
        assert!(said.contains("working"), "{said}");
    }

    #[test]
    fn plex_not_answering_is_said_plainly_and_does_not_claim_the_machine_is_broken() {
        // A machine whose Plex is down is a machine somebody can still reach, update and
        // read. Saying "FAULT" about it would send somebody to the cupboard for something
        // they could have fixed from a browser.
        let facts = facts_for(vec![plex_check(Status::Fail)], &["192.168.2.102"]);
        assert_eq!(facts.plex, Plex::Stopped);
        let (mark, said) = facts.verdict.headline();
        assert_eq!(mark, Mark::Warning);
        assert!(said.contains("Plex"), "{said}");
    }

    #[test]
    fn a_machine_with_no_address_leads_with_that_and_nothing_else() {
        // Because it is the fact that decides whether any of the others can be acted on.
        // Plex being down matters less than not being able to reach the machine to see it.
        let facts = facts_for(vec![plex_check(Status::Fail)], &[]);
        assert_eq!(facts.verdict, Verdict::NoNetwork);
        assert_eq!(facts.address(), None);
    }

    #[test]
    fn an_unprovisioned_machine_is_invited_to_be_set_up_rather_than_reported_as_broken() {
        // The state of every appliance on its first boot. "Plex: FAIL" would be the second
        // thing a new owner ever read about their machine and would be wrong.
        let facts = facts_for(vec![plex_check(Status::NotApplicable)], &["192.168.2.102"]);
        assert_eq!(facts.plex, Plex::NotInstalled);
        assert_eq!(facts.verdict, Verdict::NeedsSetup);
    }

    #[test]
    fn a_failed_health_check_is_quoted_rather_than_summarised() {
        // The gate's own words. A screen that said "unhealthy" and made somebody open a
        // browser to find out which check failed has reproduced the problem this console
        // exists to remove.
        let facts = facts_for(
            vec![
                Check {
                    name: "var-writable",
                    status: Status::Fail,
                    detail: "/var is read-only".to_owned(),
                },
                plex_check(Status::Pass),
            ],
            &["192.168.2.102"],
        );
        let Verdict::Unhealthy { failures } = &facts.verdict else {
            panic!("expected unhealthy, got {:?}", facts.verdict);
        };
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("var-writable"), "{failures:?}");
        assert!(facts.verdict.headline().1.contains("/var is read-only"));
    }

    #[test]
    fn a_recovery_leads_because_it_is_news_and_the_others_are_states() {
        // Something happened while nobody was watching. Everything else on this screen
        // describes how the machine is now; only this says how it got here.
        let recovered = |_: &str| {
            Some(crate::rollback::Record {
                version: Some("0.1.0.202608120000".to_owned()),
                slot: Some("a".to_owned()),
                tries_left: 0,
                failures: vec!["plex-http: not answering".to_owned()],
                verdict: "NOT healthy".to_owned(),
            })
        };
        let facts = Facts::from_status(
            &a_machine(vec![plex_check(Status::Pass)], &["192.168.2.102"]),
            Transcoding::Ready,
            &recovered,
        );

        assert_eq!(
            facts.verdict,
            Verdict::Recovered {
                failed: Some("0.1.0.202608120000".to_owned())
            }
        );
        assert!(facts.verdict.headline().1.contains("0.1.0.202608120000"));
    }

    #[test]
    fn a_rollback_that_no_longer_describes_this_machine_is_not_announced() {
        // The defect this console shipped for nine days, arriving on a second screen. The
        // rule is not re-implemented here -- `rollback::last_for` owns it -- so what this
        // pins is that the dashboard asks it rather than reading the record raw.
        let stale = |running: &str| {
            crate::rollback::Record {
                version: Some("0.1.0.202607010000".to_owned()),
                slot: Some("a".to_owned()),
                tries_left: 0,
                failures: Vec::new(),
                verdict: "NOT healthy".to_owned(),
            }
            .still_current(running)
            .then_some(crate::rollback::Record {
                version: Some("0.1.0.202607010000".to_owned()),
                slot: Some("a".to_owned()),
                tries_left: 0,
                failures: Vec::new(),
                verdict: "NOT healthy".to_owned(),
            })
        };
        let facts = Facts::from_status(
            &a_machine(vec![plex_check(Status::Pass)], &["192.168.2.102"]),
            Transcoding::Ready,
            &stale,
        );
        assert_eq!(
            facts.verdict,
            Verdict::Working,
            "a rollback from a release older than the running one is history"
        );
    }

    #[test]
    fn an_unknown_transcoding_verdict_is_not_reported_as_a_fault() {
        // Unknown is what this says before it has asked, which on a booting machine is a
        // real state. Turning it into "Unavailable" would put a warning about this
        // project's whole purpose on the screen on the strength of not having looked.
        assert_eq!(Transcoding::Unknown.word(), "Checking");
        // And Degraded is not Ready. The Alder Lake laptop transcoded at reduced quality
        // for weeks while every layer above reported success.
        assert_ne!(Transcoding::Degraded.word(), Transcoding::Ready.word());
        let facts = Facts::from_status(
            &a_machine(vec![plex_check(Status::Pass)], &["192.168.2.102"]),
            Transcoding::Unknown,
            &no_rollback,
        );
        assert_eq!(facts.verdict, Verdict::Working, "still working");
        assert_eq!(facts.transcoding, Transcoding::Unknown);
    }

    #[test]
    fn the_address_the_certificate_names_is_the_one_put_in_front_of_somebody() {
        // Found on the reference laptop: two adapters, and the wireless lease arrived after
        // the certificate was issued, so the screen offered an address the certificate did
        // not name. Both reach the console; what breaks is the fingerprint comparison,
        // which is the only thing that makes a self-signed certificate mean anything.
        let mut addresses = vec!["192.168.2.190".to_owned(), "192.168.2.102".to_owned()];
        prefer_covered(&mut addresses, |a| a == "192.168.2.102");
        assert_eq!(addresses, vec!["192.168.2.102", "192.168.2.190"]);

        // Stable, so this stays "which of these does the certificate name" rather than
        // becoming a second opinion about which interface is primary.
        let mut three = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        prefer_covered(&mut three, |a| a != "a");
        assert_eq!(three, vec!["b", "c", "a"]);

        // And nothing is ever dropped: an address that warns beats no address at all.
        let mut none_covered = vec!["10.0.0.5".to_owned()];
        prefer_covered(&mut none_covered, |_| false);
        assert_eq!(none_covered, vec!["10.0.0.5"]);
    }

    #[test]
    fn a_duration_is_coarse_so_that_an_idle_screen_stops_changing() {
        // The screen writes only when what it draws has changed, so a seconds-resolution
        // uptime would redraw every second and hold a laptop panel lit all night -- undoing
        // a thing this machine was asked for by name.
        assert_eq!(coarse(Duration::from_secs(20)), "less than a minute");
        assert_eq!(coarse(Duration::from_secs(60)), "1 minute");
        assert_eq!(coarse(Duration::from_secs(125)), "2 minutes");
        assert_eq!(coarse(Duration::from_secs(3_600)), "1 h 0 min");
        assert_eq!(coarse(Duration::from_secs(4_860)), "1 h 21 min");
        assert_eq!(coarse(Duration::from_secs(90_000)), "1 day 1 h");
        assert_eq!(coarse(Duration::from_secs(200_000)), "2 days 7 h");
    }

    #[test]
    fn the_duration_says_the_value_and_not_what_it_is() {
        // UP UP 18 MIN was on a real machine for a day, because a formatter that named
        // itself in its own output cannot be composed.
        for seconds in [30, 600, 90_000] {
            let said = coarse(Duration::from_secs(seconds));
            assert!(!said.to_lowercase().contains("up"), "{said}");
        }
    }
}
