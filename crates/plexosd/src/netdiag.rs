//! The three network questions that have each cost a trip to the attached screen.
//!
//! Interface state is already on the status page, and it has never been the thing that
//! was wrong. What has been wrong, three times, is a layer above it: a resolver the
//! appliance could not read, a route that was not there, and a name that would not
//! resolve on a machine whose link was demonstrably fine. Each diagnosis needed somebody
//! sitting at a 2160x1440 panel reading a shell.
//!
//! # Why a route separate from `/api/status`
//!
//! The name lookup blocks. musl's resolver waits seconds per nameserver before giving up,
//! and the status page polls — so folding this into it would stall the one view that has
//! to keep answering while the machine is unwell. This is asked for, not polled.
//!
//! # `resolv.conf` is a symlink, and that is the whole story of one outage
//!
//! Buildroot's skeleton makes `/etc/resolv.conf` a symlink to `../run/resolv.conf` so a
//! read-only `/etc` can still have a lease-managed resolver. Landlock resolves symlinks
//! and checks the *target*, so granting `/etc` did not grant the file, musl fell back to
//! `127.0.0.1` where nothing listens, and every lookup failed while DNS from a shell was
//! fine. That is why this reports the link and its target rather than just the contents:
//! the contents looked perfect throughout.
//!
//! # What has run
//!
//! **Nothing on hardware.** The parsers are covered by tests against captured output;
//! no appliance has served this yet.

use std::path::Path;

/// Where the resolver configuration lives.
const RESOLV_CONF: &str = "/etc/resolv.conf";

/// The kernel's routing table, in the only form this image can read it.
///
/// `ip route` would be easier to read and is a program that may or may not be in the
/// image — busybox's `ip` has been a source of surprises here twice already. `/proc` is
/// always there and never a different build.
const PROC_ROUTE: &str = "/proc/net/route";

/// The name resolved to prove DNS works.
///
/// `downloads.plex.tv`, not `example.com`: it is the host ADR-0010 actually fetches Plex
/// from, so a success here means the thing the appliance needs is reachable rather than
/// that some unrelated name resolved. A diagnostic that passes while the real dependency
/// fails is worse than none.
pub const PROBE_HOST: &str = "downloads.plex.tv";

/// How long the name lookup is given before it is reported as a failure.
///
/// musl tries each nameserver in turn with its own timeout, so a machine pointed at a
/// dead resolver can sit here for a long time. The page is waiting on this.
pub const LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// What `GET /api/network` reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Diagnosis {
    /// The resolver configuration, and how it is reached.
    pub resolver: Resolver,
    /// The default route, if there is one.
    pub default_route: Option<Route>,
    /// Whether a name actually resolves.
    pub lookup: Lookup,
    /// What to do about whatever is wrong, in the order worth trying.
    ///
    /// Assembled here rather than left to the page, so that a reader of the JSON gets the
    /// same help as a reader of the console.
    pub remedies: Vec<String>,
}

/// The resolver configuration as the appliance sees it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Resolver {
    /// The path consulted.
    pub path: String,
    /// What it is a symlink to, if it is one.
    ///
    /// Reported because a granted directory does not grant a file that points out of it,
    /// which is how DNS once failed on a machine whose `resolv.conf` was perfect.
    pub symlink_to: Option<String>,
    /// Nameserver addresses, in the order the resolver will try them.
    pub nameservers: Vec<String>,
    /// Why it could not be read, if it could not.
    pub error: Option<String>,
}

/// A default route.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Route {
    /// The interface packets leave by.
    pub interface: String,
    /// The gateway they are handed to.
    pub gateway: String,
}

/// The outcome of resolving [`PROBE_HOST`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum Lookup {
    /// The name resolved, to these addresses.
    Resolved {
        /// The host that was looked up.
        host: String,
        /// What it resolved to.
        addresses: Vec<String>,
    },
    /// The resolver answered, and the answer was a failure.
    Failed {
        /// The host that was looked up.
        host: String,
        /// What the resolver said.
        error: String,
    },
    /// The resolver did not answer within [`LOOKUP_TIMEOUT`].
    TimedOut {
        /// The host that was looked up.
        host: String,
    },
}

/// Reads `/etc/resolv.conf` and reports both its contents and how it is reached.
#[must_use]
pub fn resolver(path: &Path) -> Resolver {
    // symlink_metadata, not metadata: the whole point is to see the link rather than
    // follow it. `metadata` would report the target and hide the thing that broke DNS.
    let symlink_to = std::fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_symlink)
        .and_then(|_| std::fs::read_link(path).ok())
        .map(|t| t.to_string_lossy().into_owned());

    match std::fs::read_to_string(path) {
        Ok(contents) => Resolver {
            path: path.display().to_string(),
            symlink_to,
            nameservers: parse_nameservers(&contents),
            error: None,
        },
        Err(error) => Resolver {
            path: path.display().to_string(),
            symlink_to,
            nameservers: Vec::new(),
            error: Some(error.to_string()),
        },
    }
}

/// Pulls the nameserver addresses out of a `resolv.conf`.
#[must_use]
pub fn parse_nameservers(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.starts_with(';'))
        .filter_map(|l| l.strip_prefix("nameserver"))
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Finds the default route in `/proc/net/route`.
///
/// The destination and gateway columns are little-endian hex of a network-order address,
/// which is to say they are byte-reversed twice and read backwards. A destination of all
/// zeroes is the default route.
#[must_use]
pub fn parse_default_route(contents: &str) -> Option<Route> {
    for line in contents.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let interface = fields.next()?;
        let destination = fields.next()?;
        let gateway = fields.next()?;

        if destination.eq_ignore_ascii_case("00000000") {
            return Some(Route {
                interface: interface.to_owned(),
                gateway: hex_address(gateway)?,
            });
        }
    }
    None
}

/// Turns `/proc/net/route`'s little-endian hex into dotted quad.
fn hex_address(hex: &str) -> Option<String> {
    let raw = u32::from_str_radix(hex, 16).ok()?;
    let [a, b, c, d] = raw.to_le_bytes();
    Some(format!("{a}.{b}.{c}.{d}"))
}

/// Resolves [`PROBE_HOST`], bounded by [`LOOKUP_TIMEOUT`].
///
/// On a thread with a channel, because the resolver has no timeout of its own that this
/// can set and a page waiting forever is a page that is broken in a way nobody can
/// diagnose — which would be an unusually poor outcome for a diagnostic.
///
/// A thread left behind on timeout is deliberate and harmless: it finishes eventually and
/// sends into a channel nobody is listening to.
#[must_use]
pub fn lookup(host: &str) -> Lookup {
    use std::net::ToSocketAddrs as _;

    let (tx, rx) = std::sync::mpsc::channel();
    let target = format!("{host}:443");
    std::thread::spawn(move || {
        let _ = tx.send(
            target
                .to_socket_addrs()
                .map(|addrs| addrs.map(|a| a.ip().to_string()).collect::<Vec<_>>())
                .map_err(|e| e.to_string()),
        );
    });

    match rx.recv_timeout(LOOKUP_TIMEOUT) {
        Ok(Ok(addresses)) => Lookup::Resolved {
            host: host.to_owned(),
            addresses,
        },
        Ok(Err(error)) => Lookup::Failed {
            host: host.to_owned(),
            error,
        },
        Err(_) => Lookup::TimedOut {
            host: host.to_owned(),
        },
    }
}

/// Gathers all three, and works out what to say about them.
#[must_use]
pub fn gather() -> Diagnosis {
    let resolver = resolver(Path::new(RESOLV_CONF));
    let default_route =
        parse_default_route(&std::fs::read_to_string(PROC_ROUTE).unwrap_or_default());
    let lookup = lookup(PROBE_HOST);

    let remedies = remedies(&resolver, default_route.as_ref(), &lookup);

    Diagnosis {
        resolver,
        default_route,
        lookup,
        remedies,
    }
}

/// What to try, in the order worth trying it.
///
/// Ordered by layer rather than by severity: a missing route makes DNS fail too, so
/// telling somebody to look at their nameservers first would send them to the symptom.
#[must_use]
pub fn remedies(resolver: &Resolver, route: Option<&Route>, lookup: &Lookup) -> Vec<String> {
    let mut remedies = Vec::new();

    if route.is_none() {
        remedies.push(
            "There is no default route, so nothing can leave this network. The DHCP \
             lease either did not arrive or carried no router. Check the cable and the \
             interface list above; everything below this line will fail until it is \
             fixed."
                .to_owned(),
        );
    }

    if let Some(error) = &resolver.error {
        remedies.push(format!(
            "{} could not be read ({error}). {}",
            resolver.path,
            if resolver.symlink_to.is_some() {
                "It is a symlink, and the likeliest cause is that its target is outside \
                 what the confinement policy grants -- granting the directory the link \
                 points into is the fix, not granting the file."
            } else {
                "udhcpc writes this on every lease; a missing file means no lease has \
                 been taken."
            }
        ));
    } else if resolver.nameservers.is_empty() {
        remedies.push(format!(
            "{} names no nameservers, so every lookup fails immediately. udhcpc writes \
             them from the DHCP lease -- if the lease is good, its script did not run.",
            resolver.path
        ));
    }

    match lookup {
        Lookup::Resolved { .. } => {}
        Lookup::Failed { host, error } => remedies.push(format!(
            "{host} did not resolve ({error}). If the route and nameservers above look \
             right, the resolver is being answered by something that is not a resolver, \
             or the appliance cannot reach it."
        )),
        Lookup::TimedOut { host } => remedies.push(format!(
            "{host} did not resolve within {}s -- the nameservers listed above are not \
             answering at all, rather than answering with a failure. That is a \
             reachability problem, not a DNS one.",
            LOOKUP_TIMEOUT.as_secs()
        )),
    }

    if remedies.is_empty() {
        remedies.push("Nothing to fix: a route, nameservers, and a name that resolves.".to_owned());
    }

    remedies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nameservers_are_read_in_order_and_comments_ignored() {
        let contents = "# generated by udhcpc\nnameserver 192.168.2.1\n\
                        ; a semicolon comment\nnameserver 8.8.8.8\nsearch lan\n";
        assert_eq!(
            parse_nameservers(contents),
            ["192.168.2.1".to_owned(), "8.8.8.8".to_owned()]
        );
    }

    #[test]
    fn a_resolv_conf_with_no_nameservers_yields_none_rather_than_a_blank() {
        // The state musl falls back to 127.0.0.1 from, which is how DNS failed on a
        // machine whose networking was fine. An empty list has to be distinguishable
        // from a list that was never read.
        assert!(parse_nameservers("search lan\n").is_empty());
    }

    #[test]
    fn the_default_route_is_read_out_of_proc() {
        // Captured shape from /proc/net/route. The gateway is little-endian hex, so
        // 0102A8C0 is 192.168.2.1 -- reading it the other way round gives 1.2.168.192,
        // which is a plausible-looking address and completely wrong.
        let contents = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
             eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\n\
             eth0\t00000000\t0102A8C0\t0003\t0\t0\t0\t00000000\n";

        assert_eq!(
            parse_default_route(contents),
            Some(Route {
                interface: "eth0".to_owned(),
                gateway: "192.168.2.1".to_owned(),
            })
        );
    }

    #[test]
    fn a_table_with_no_default_route_reports_none() {
        let contents = "Iface\tDestination\tGateway\n\
                        eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\n";
        assert_eq!(parse_default_route(contents), None);
    }

    #[test]
    fn a_missing_route_is_reported_before_anything_that_depends_on_it() {
        // Ordering by layer, not by severity. With no route the lookup fails too, and a
        // reader told to check nameservers first is being sent to the symptom.
        let resolver = Resolver {
            path: "/etc/resolv.conf".to_owned(),
            symlink_to: None,
            nameservers: vec![],
            error: None,
        };
        let lookup = Lookup::TimedOut {
            host: "downloads.plex.tv".to_owned(),
        };

        let remedies = remedies(&resolver, None, &lookup);
        assert!(remedies[0].contains("no default route"), "{remedies:?}");
        assert_eq!(remedies.len(), 3, "all three layers say something");
    }

    #[test]
    fn an_unreadable_symlink_names_the_landlock_shape_of_the_problem() {
        // The outage this module exists because of. The file was fine; the policy
        // granted /etc and the link pointed into /run, and musl reported none of it.
        let resolver = Resolver {
            path: "/etc/resolv.conf".to_owned(),
            symlink_to: Some("../run/resolv.conf".to_owned()),
            nameservers: vec![],
            error: Some("Permission denied".to_owned()),
        };

        let remedies = remedies(
            &resolver,
            Some(&Route {
                interface: "eth0".to_owned(),
                gateway: "192.168.2.1".to_owned(),
            }),
            &Lookup::Resolved {
                host: "x".to_owned(),
                addresses: vec![],
            },
        );

        assert_eq!(remedies.len(), 1);
        assert!(remedies[0].contains("symlink"), "{remedies:?}");
        assert!(
            remedies[0].contains("directory the link points into"),
            "and names the fix rather than the symptom: {remedies:?}"
        );
    }

    #[test]
    fn a_healthy_machine_still_says_something() {
        // A diagnostic that prints nothing when all is well leaves the reader unsure
        // whether it ran.
        let remedies = remedies(
            &Resolver {
                path: "/etc/resolv.conf".to_owned(),
                symlink_to: Some("../run/resolv.conf".to_owned()),
                nameservers: vec!["192.168.2.1".to_owned()],
                error: None,
            },
            Some(&Route {
                interface: "eth0".to_owned(),
                gateway: "192.168.2.1".to_owned(),
            }),
            &Lookup::Resolved {
                host: "downloads.plex.tv".to_owned(),
                addresses: vec!["1.2.3.4".to_owned()],
            },
        );

        assert_eq!(remedies.len(), 1);
        assert!(remedies[0].starts_with("Nothing to fix"), "{remedies:?}");
    }

    #[test]
    fn the_probe_host_is_the_one_the_appliance_actually_depends_on() {
        // A lookup of example.com that succeeds while downloads.plex.tv fails is a
        // diagnostic that reports health during an outage.
        assert_eq!(PROBE_HOST, "downloads.plex.tv");
    }
}
