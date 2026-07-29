//! The status console: the routes, and the page.
//!
//! This is the first thing in PlexOS a person interacts with rather than reads off a
//! kernel console, and its job is narrow: answer "is this machine working, and if not,
//! what do I do about it" from a browser on another device.
//!
//! # It shows, and now it does
//!
//! Reading needs nothing: the status, the GPU verdict and the health check answer any
//! browser on the LAN, because a console that demanded a credential before it would say
//! why a boot failed would defeat the reason it exists.
//!
//! Changing anything needs the device token. `POST /api/provision` installs Plex, and it
//! is [`http::route`] rather than this module that refuses it without a credential — the
//! check sits in front of the whole route table, so a route added here is authenticated
//! whether or not its author thought about it. ADR-0013 describes where the token comes
//! from; [`claim`] is what issues it, on the console attached to the machine, at first
//! start.
//!
//! # Binding
//!
//! Port 80 by default, because the point is that someone types an address and gets a
//! page — `http://192.168.2.42` and not a port number remembered from a document. It
//! binds all interfaces: the appliance is expected to be reached from other machines
//! on the LAN, and a console reachable only from the machine it describes would be
//! useless on hardware with no browser.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};

use plexos_gpu::env::{Environment, System};

use crate::http::{self, Request, Response};
use crate::net;
use crate::status::Status;

/// The page itself. See the comment at the top of it for why it is embedded.
pub const PAGE: &str = include_str!("ui/console.html");

/// Default port. See the module documentation.
pub const DEFAULT_PORT: u16 = 80;

/// Answers one request, against the machine described by `env`.
///
/// Separated from the socket so the whole route table can be tested against a recorded
/// machine, which is the same boundary every other module here draws.
///
/// `job` carries the state of any provisioning run. It is reached from every request
/// because `GET /api/provision` is how the page follows one, and a request that started
/// an installation returns before that installation is anywhere near finished.
///
/// This function does **not** check the device token. [`http::route`] does, before this
/// is called, for every method that is not a read — so a route added here is
/// authenticated by construction rather than by its author remembering to be.
#[must_use]
pub fn respond(
    request: &Request,
    env: &impl Environment,
    job: &std::sync::Arc<crate::provision::Job>,
    plex: &std::sync::Arc<crate::plex::Handle>,
    update: &std::sync::Arc<crate::update::Job>,
) -> Response {
    match (request.method.as_str(), request.path.as_str()) {
        // Starting an installation. Returns as soon as the work is handed to a thread:
        // the download alone is minutes, and a request held open for it would time out
        // in the browser with the install still running and no way to say so.
        ("POST", "/api/provision") => {
            if !job.begin() {
                return Response::text(
                    409,
                    "An installation is already running on this machine. Watch it at \
                     GET /api/provision; starting a second would unpack into the same \
                     directory as the first.\n",
                );
            }
            // The job that was just claimed, not a fresh one: the whole point of
            // begin() is that the thread reports into the state the page is polling.
            crate::provision::spawn(
                job,
                plex,
                std::path::PathBuf::from(plexos_types::paths::PLEX_APPS),
                std::path::PathBuf::from(plexos_plex::verify::PLEX_KEYRING),
            );
            Response::json("{\"started\":true}")
        }

        // Checking for, and installing, a new /usr. Same shape as provisioning and for
        // the same reason: the work is minutes and a request cannot be held open for it.
        ("POST", "/api/update") => {
            if !update.begin() {
                return Response::text(
                    409,
                    "An update is already running. Watch it at GET /api/update.\n",
                );
            }
            let install = crate::update::wants_install(&request.body);
            crate::update::spawn(update, crate::update::source_in(&request.body), install);
            Response::json(format!("{{\"install\":{install}}}"))
        }

        ("GET" | "HEAD", "/api/update") => match serde_json::to_string(&update.snapshot()) {
            Ok(json) => Response::json(json),
            Err(error) => Response::text(500, format!("could not serialise progress: {error}\n")),
        },

        // Network shares: the library lives on a NAS, and without one there is nothing
        // to play.
        ("GET" | "HEAD", "/api/shares") => match serde_json::to_string(&crate::shares::states()) {
            Ok(json) => Response::json(json),
            Err(error) => Response::text(500, format!("could not serialise shares: {error}\n")),
        },

        ("POST", "/api/shares") => crate::shares::handle(&request.body),

        // Stopping the machine. The response goes out before anything happens, because
        // reboot(2) does not return -- see crate::power for why that ordering matters.
        ("POST", "/api/power") => match crate::power::action_in(&request.body) {
            Some(action) => {
                crate::power::schedule(action, plex);
                Response::json(format!(
                    "{{\"action\":\"{}\"}}",
                    match action {
                        plexos_sys::power::Action::Off => "off",
                        plexos_sys::power::Action::Restart => "restart",
                    }
                ))
            }
            None => Response::text(
                400,
                "Send {\"action\":\"off\"} or {\"action\":\"restart\"}. Nothing is \
                 assumed when the action is missing or unrecognised: guessing wrong \
                 either leaves a machine running that was meant to be silent, or takes \
                 down one somebody wanted back.\n",
            ),
        },

        // Following one. Polled by the page every second or so, so it is deliberately
        // cheap: it reads a struct behind a mutex and serialises it.
        ("GET" | "HEAD", "/api/provision") => {
            let mount = std::path::Path::new(plexos_types::paths::PLEX_MOUNT);
            let report = crate::provision::Report {
                progress: job.snapshot(),
                installed: crate::plex::is_provisioned(mount),
                running: plex.is_running(),
                plex_log: plex.log(),
                web: crate::provision::PLEX_WEB,
            };
            match serde_json::to_string(&report) {
                Ok(json) => Response::json(json),
                Err(error) => {
                    Response::text(500, format!("could not serialise progress: {error}\n"))
                }
            }
        }

        (_, path) => respond_read_only(request, env, path),
    }
}

/// The routes that only ever report.
fn respond_read_only(request: &Request, env: &impl Environment, path: &str) -> Response {
    let _ = request;
    match path {
        "/" | "/index.html" => Response::html(PAGE),

        "/api/status" => match Status::gather(env).to_json() {
            Ok(json) => Response::json(json),
            Err(error) => Response::text(500, format!("could not serialise status: {error}\n")),
        },

        // The GPU verdict on its own, so a script can ask the one question the project
        // exists to answer without parsing the whole document.
        "/api/gpu" => match serde_json::to_string_pretty(&Status::gather(env).gpu) {
            Ok(json) => Response::json(json),
            Err(error) => Response::text(500, format!("could not serialise report: {error}\n")),
        },

        // Plain text and a status code, for anything that checks rather than reads.
        "/healthz" => {
            let health = crate::health::run_all();
            if health.is_healthy() {
                Response::text(200, "healthy\n")
            } else {
                let detail = health.failures().iter().fold(String::new(), |mut out, c| {
                    use std::fmt::Write as _;
                    let _ = writeln!(out, "{}: {}", c.name, c.detail);
                    out
                });
                Response::text(500, format!("NOT healthy\n{detail}"))
            }
        }

        other => Response::text(
            404,
            format!(
                "no such page: {other}\n\nThis console serves / , /api/status, /api/gpu, \
                 /healthz, /api/provision, /api/update, /api/shares and /api/power. \
                 Everything that changes something takes POST and the device token.\n"
            ),
        ),
    }
}

/// Reads the device's credential, claiming the device if it has none.
///
/// This is the moment ADR-0013 describes: a device with no credential generates one,
/// stores only its fingerprint, and shows the token itself exactly once, on the console
/// physically attached to the machine. Whoever can read that screen becomes the
/// administrator. There is no other way in, and no default credential to forget to
/// change.
///
/// Shown once and never again is deliberate. Only the fingerprint is kept, so nothing on
/// the appliance can reprint the token — which is the same property that makes a stolen
/// copy of `/var` useless. Losing it is recoverable by deleting
/// [`crate::auth::CREDENTIAL_FILE`] and restarting, and the banner says so, because an
/// administrator who has lost the token will otherwise conclude the appliance is bricked.
///
/// # Failing closed
///
/// If the token cannot be generated or stored, this returns [`Credential::Unset`], which
/// makes every mutating route answer 503. The alternative — carrying on with a token that
/// was never written — would leave a device that accepts a credential now and rejects it
/// after a reboot, which is worse than one that plainly cannot be claimed yet.
///
/// [`Credential::Unset`]: crate::auth::Credential::Unset
pub fn claim(path: &std::path::Path, log: &mut dyn FnMut(&str)) -> crate::auth::Credential {
    use crate::auth::Credential;

    if let Credential::Set(fingerprint) = crate::auth::read(path) {
        log("device claimed; changes need its token");
        return Credential::Set(fingerprint);
    }

    let token = match crate::auth::generate() {
        Ok(token) => token,
        Err(error) => {
            log(&format!(
                "could not generate a device token: {error}. Nothing may change this \
                 machine until one exists. /dev/urandom is unreadable, which means /dev \
                 is not mounted -- a larger fault than the missing token."
            ));
            return Credential::Unset;
        }
    };

    let fingerprint = crate::auth::fingerprint(&token);
    if let Err(error) = crate::auth::write(path, &fingerprint) {
        // Deliberately without printing the token that was generated. Showing one the
        // device will not accept after a restart is worse than showing none: whoever
        // wrote it down would spend the next hour blaming the token.
        log(&format!(
            "could not store the device credential at {}: {error}. No token has been \
             issued, and nothing may change this machine until one can be. Check that \
             /var is mounted and writable.",
            path.display()
        ));
        return Credential::Unset;
    }

    // Banner rather than a log line. This is the one secret the machine will ever show,
    // it is shown once, and it has to be findable in a scrollback of boot messages on a
    // 2160x1440 panel — which is also why the token is sixteen characters in four
    // groups rather than sixty-four unbroken ones.
    log("");
    log("================================================================");
    log("");
    log("  This device is now claimed. Its token, shown only this once:");
    log("");
    log(&format!(
        "        {}",
        crate::auth::grouped(&token).replace('-', " - ")
    ));
    log("");
    log("  16 characters. There is no O, I, L or U, so nothing here is");
    log("  ambiguous: a 0 is always a zero and a 1 is always a one.");
    log("  Case does not matter. Type it with or without the dashes.");
    log("");
    log("  Type it into the console page to install or change anything.");
    log("  Reading the status page needs nothing.");
    log(&format!(
        "  Lost it? Delete {} and restart to be issued another.",
        path.display()
    ));
    log("");
    log("================================================================");
    log("");

    Credential::Set(fingerprint)
}

/// Writes down why this boot is about to hand itself back to the other slot.
///
/// Reads the version and slot from the same two sources [`crate::status`] does, because a
/// record that disagreed with the status page about which version failed would be worse
/// than no record. Both are optional: an `/etc/os-release` that cannot be read is a
/// reason to write a partial note, not to skip the note.
///
/// Every failure here is logged and swallowed. A rollback that happens without a note is
/// bad; a rollback that does not happen because a note could not be written is worse, and
/// the caller is one line from `reboot(2)`.
fn record_rollback(verdict: &crate::gate::Verdict, log: &mut dyn FnMut(&str)) {
    let crate::gate::Verdict::Unhealthy { failures, trial } = verdict else {
        return;
    };
    let tries_left = match trial {
        crate::gate::Trial::Counting { tries_left } => *tries_left,
        _ => 0,
    };

    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();

    let record = crate::rollback::Record {
        version: crate::status::os_release_value(&os_release, "VERSION_ID"),
        slot: crate::status::cmdline_value(&cmdline, crate::status::KEY_SLOT),
        tries_left,
        failures: failures.clone(),
        verdict: verdict.to_string(),
    };

    let path = std::path::Path::new(plexos_types::paths::ROLLBACK_RECORD_FILE);
    match crate::rollback::write(&record, path) {
        Ok(()) => log(&format!(
            "why this boot failed is recorded in {}, which rollback does not revert",
            path.display()
        )),
        Err(error) => log(&format!(
            "could not record why this boot failed in {}: {error}. The rollback still \
             happens; what is lost is the explanation, so the machine will come back on \
             the older slot with nothing saying why.",
            path.display()
        )),
    }
}

/// Brings the network up, then serves the console until the listener fails.
///
/// The network is configured first and its failure is **reported, not propagated**: a
/// machine with no cable should still serve the console to anyone who reaches it by
/// another route, and more importantly should still be running so the console on the
/// machine itself can say why.
///
/// The health gate runs from *inside* this function, on a thread, because the gate asks
/// whether Plex is answering and this is the process that starts Plex. So a rollback can
/// begin here — which is the opposite of what this comment said while the gate still ran
/// in `plexos-init`, and is worth stating plainly because the ordering has now been
/// wrong in both directions.
///
/// # Errors
/// Fails only if the port cannot be bound — almost always because something else holds
/// it, or because the daemon is not running as root and the port is below 1024.
pub fn run(port: u16, log: &mut dyn FnMut(&str)) -> io::Result<()> {
    let configured = match net::configure(&System, net::LINK_TIMEOUT, log) {
        Ok(interface) => {
            log(&format!("network configured on {}", interface.name));
            Some(interface)
        }
        Err(error) => {
            log(&format!("no network: {error}"));
            None
        }
    };

    let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let listener = TcpListener::bind(address).map_err(|error| {
        // The remedy has to match the cause. "Try a higher port" is good advice for
        // EACCES and actively misleading for EADDRINUSE, where the port is fine and
        // something else is holding it.
        let remedy = match error.kind() {
            io::ErrorKind::PermissionDenied => {
                "Ports below 1024 need root, and this is not running as root. Either \
                 start it as root or pass --port with a number above 1024."
                    .to_owned()
            }
            io::ErrorKind::AddrInUse => format!(
                "Something is already listening on port {port}. Find it with \
                 `netstat -tlnp | grep {port}`, or pass --port with a free one."
            ),
            _ => "Check that the address is one this machine can bind.".to_owned(),
        };
        io::Error::new(
            error.kind(),
            format!("could not bind {address}: {error}. {remedy}"),
        )
    })?;

    log(&format!("console listening on {address}"));

    // After binding, so the socket exists as early as possible, and after configure
    // rather than inside it, because udhcpc is spawned and never waited on. Printing
    // the URL without this waiting step prints it before any lease can exist, which is
    // to say never — the console worked and said nothing a person could act on.
    if let Some(interface) = configured {
        match net::wait_for_address(&System, &interface.name, net::LEASE_TIMEOUT, log) {
            Some(found) => log(&format!("console at http://{}/", found.ip())),
            None => log(&format!(
                "{} is up but DHCP produced no address in {}s. The console is serving on \
                 port {port} and unreachable until the interface has one. Check for a \
                 DHCP server on this segment, or set an address by hand with \
                 `ip addr add <a.b.c.d/nn> dev {}`.",
                interface.name,
                net::LEASE_TIMEOUT.as_secs(),
                interface.name
            )),
        }
    }

    // The credential is read once, here, and what it is decides how the console
    // behaves rather than merely what it logs: an unclaimed device refuses every
    // mutating route outright (ADR-0013).
    let credential = claim(std::path::Path::new(crate::auth::CREDENTIAL_FILE), log);

    // One job and one Plex for the life of the daemon, shared by every connection thread.
    // Both are properties of the machine rather than of whoever asked: a second browser
    // must see the installation the first one started, not an idle console.
    let job = std::sync::Arc::new(crate::provision::Job::new());
    let plex = std::sync::Arc::new(crate::plex::Handle::new());
    let update = std::sync::Arc::new(crate::update::Job::new());

    // Before Plex, not after. The Landlock policy is built when Plex starts, from the
    // paths that exist then; mounting a library afterwards may leave it unreachable, and
    // that is a question this project has already been caught guessing at.
    crate::shares::mount_all(log);

    // Before serving, so a machine that was provisioned on an earlier boot is running
    // Plex by the time anyone loads the page. On an unprovisioned one this says so and
    // costs nothing.
    plex.ensure_started(std::path::Path::new(plexos_types::paths::PLEX_MOUNT), log);

    // ARCHITECTURE.md §2 step 7, and it has to be here: the gate asks whether Plex is
    // answering, and this is the process that starts Plex. It ran before, in plexos-init,
    // and therefore always failed on a provisioned machine -- the counter was never
    // cleared and ADR-0005 stopped meaning anything.
    //
    // On a thread, because waiting for Plex takes seconds and the console is the only
    // tool for finding out why a machine is unwell. It must not wait for the machine to
    // be well before it will say anything.
    // The gate's thread owns a Plex handle because an unhealthy verdict may end in a
    // restart, and a restart has to stop Plex and get /var read-only first. Rolling back
    // by cutting power to a mounted XFS would trade a bad update for a bad database.
    let gate_plex = std::sync::Arc::clone(&plex);
    std::thread::spawn(move || {
        let mut log = |line: &str| println!("plexosd: gate: {line}");
        let verdict = crate::gate::run_after_plex(
            std::path::Path::new(plexos_types::paths::PLEX_APPS),
            None,
            &mut log,
        );
        log(&verdict.to_string());

        // ADR-0005's other half, and the one that has never run. The counter is spent by
        // booting, so leaving it standing achieves nothing on a machine that does not
        // reboot -- which is every machine, because nothing here rebooted. The verdict
        // decides; `demands_restart` is true only for an entry the bootloader is still
        // counting, so a permanent slot stays up and diagnosable.
        if verdict.demands_restart() {
            // Before the restart, because there is no after: stop_now does not return.
            // The record goes on /var, which is the only surface a rollback leaves alone
            // -- everything else that could explain this boot is in the /usr about to be
            // rolled away, so it vanishes exactly when somebody wants it.
            record_rollback(&verdict, &mut log);

            log("restarting to spend a try");
            crate::power::stop_now(plexos_sys::power::Action::Restart, &gate_plex, &mut log);
        }
    });

    let served_job = std::sync::Arc::clone(&job);
    http::serve(
        &listener,
        credential,
        move |request| respond(request, &System, &served_job, &plex, &update),
        log,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_gpu::env::Fixture;

    /// `respond` against a console that has never provisioned anything.
    fn respond_test(request: &Request, env: &impl Environment) -> Response {
        respond(
            request,
            env,
            &std::sync::Arc::new(crate::provision::Job::new()),
            &std::sync::Arc::new(crate::plex::Handle::new()),
            &std::sync::Arc::new(crate::update::Job::new()),
        )
    }

    fn get(path: &str) -> Request {
        Request {
            method: "GET".to_owned(),
            path: path.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn the_root_path_serves_the_page() {
        let response = respond_test(&get("/"), &Fixture::new());
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert!(response.body.starts_with(b"<!doctype html>"));
    }

    #[test]
    fn the_page_fetches_the_endpoint_it_is_served_alongside() {
        // The page and the route table are edited independently. If they drift, the
        // console renders its error state on a perfectly healthy machine, and the
        // symptom points at the daemon rather than at this mismatch.
        assert!(
            PAGE.contains("\"/api/status\""),
            "the page must fetch the route respond() actually serves"
        );
        assert_eq!(
            respond_test(&get("/api/status"), &Fixture::new()).status,
            200
        );
    }

    #[test]
    fn the_page_carries_no_external_references() {
        // /usr is read-only and the appliance may have no route off the LAN. Anything
        // fetched from elsewhere renders this page unstyled in exactly the situation
        // it exists for: a machine whose network is broken.
        for marker in ["https://", "<script src", "<link", "<img", "@import"] {
            assert!(
                !PAGE.contains(marker),
                "the page must be self-contained, but it contains {marker:?}"
            );
        }
    }

    #[test]
    fn the_token_field_outlives_the_flow_that_first_needed_it() {
        // It began inside the Plex install card, which renders as a single link once Plex
        // is running -- so on a working appliance there was nowhere to type the token,
        // and every button that needs one silently refused. Reported as "I click Check
        // and nothing happens".
        assert!(
            PAGE.contains("id=\"token-card\""),
            "the token needs a place of its own, not a corner of a flow that finishes"
        );
        let card = PAGE.find("id=\"token-card\"").expect("the card");
        let plex = PAGE.find("id=\"plex\"").expect("the Plex card");
        assert!(
            card < plex,
            "and it has to come before the things that need it"
        );
    }

    #[test]
    fn a_failure_is_shown_in_the_section_whose_button_was_pressed() {
        // Writing every error into the Plex card put messages three sections away from
        // the button that caused them.
        assert!(
            PAGE.contains("sectionError(\"update\""),
            "update errors go to update"
        );
        assert!(
            PAGE.contains("sectionError(\"plex\""),
            "and Plex errors to Plex"
        );
    }

    #[test]
    fn the_page_sends_the_token_the_way_the_adr_requires() {
        // ADR-0013 chose a bearer token so that nothing is attached to a request
        // automatically. A page that put it in a cookie or a query string would defeat
        // that: the first is sent on every request whether or not it should be, and the
        // second lands in every proxy log and browser history on the way.
        assert!(
            PAGE.contains("\"Authorization\": \"Bearer \" + value"),
            "the page must present the token as an Authorization: Bearer header"
        );
        assert!(!PAGE.contains("document.cookie"), "and not as a cookie");
        assert!(
            !PAGE.contains("?token="),
            "and not in a query string, which is logged everywhere"
        );
    }

    #[test]
    fn stopping_the_machine_refuses_a_request_that_does_not_say_which_way() {
        // No safe default: guessing restart when somebody meant off leaves a machine
        // running that was meant to be silent, and guessing the other way takes down a
        // server somebody wanted back. Note this never reaches schedule(), so no test
        // here can turn the build host off.
        for body in [&b"{}"[..], b"", br#"{"action":"halt"}"#] {
            let request = Request {
                method: "POST".to_owned(),
                path: "/api/power".to_owned(),
                headers: Vec::new(),
                body: body.to_vec(),
            };
            let response = respond(
                &request,
                &Fixture::new(),
                &std::sync::Arc::new(crate::provision::Job::new()),
                &std::sync::Arc::new(crate::plex::Handle::new()),
                &std::sync::Arc::new(crate::update::Job::new()),
            );
            assert_eq!(response.status, 400, "{body:?}");
        }
    }

    #[test]
    fn the_page_can_add_a_media_share() {
        // Without one there is nothing to play, and the appliance's own /var is far too
        // small to be anybody's library.
        assert!(PAGE.contains("\"/api/shares\""));
        assert!(PAGE.contains("Add and mount"));
        assert!(
            PAGE.contains("read-only"),
            "the page must say a library is never written to"
        );
        assert!(
            PAGE.contains("has to be restarted before it can see it"),
            "and must say why adding a share is not enough on its own"
        );
    }

    #[test]
    fn the_page_can_check_for_and_install_a_system_update() {
        assert!(
            PAGE.contains("\"/api/update\""),
            "pointed at the route this serves"
        );
        assert!(PAGE.contains("Download and install"));
        assert!(
            PAGE.contains("Nothing signs this bundle"),
            "the page must say what it does not know, because the reader cannot"
        );
        assert!(
            PAGE.contains("comes back to this one by itself"),
            "and must say what happens when an update is bad, which is the question \
             anybody hesitating over that button is actually asking"
        );
    }

    #[test]
    fn the_page_offers_a_way_to_stop_that_is_not_the_power_button() {
        // The machine has no keyboard worth using and no shell anybody is expected to
        // reach. Holding the power button for five seconds cuts power mid-write.
        assert!(PAGE.contains("Shut down"), "the page must offer a shutdown");
        assert!(PAGE.contains("Restart"), "and a restart");
        assert!(
            PAGE.contains("\"/api/power\""),
            "pointed at the route this module serves"
        );
        assert!(
            PAGE.contains("confirm("),
            "behind a confirmation, because the mistake is expensive"
        );
    }

    #[test]
    fn the_page_does_not_redraw_the_field_somebody_is_typing_into() {
        // Reported from the appliance: polling replaced the Plex section wholesale every
        // few seconds, and the token field went with it, so the caret jumped out roughly
        // once per word. The token is sixteen characters read off another screen, which
        // makes that close to unusable.
        //
        // Asserted on the page's text because there is no JavaScript engine here. It is a
        // weak test and it is the one available; the property it guards is that a redraw
        // is conditional at all.
        assert!(
            PAGE.contains("if (signature === lastRendered) return;"),
            "the Plex section must not be rebuilt when nothing has changed"
        );
        assert!(
            PAGE.contains("setSelectionRange"),
            "and when it is rebuilt, the caret must be put back"
        );
    }

    #[test]
    fn the_page_drives_the_routes_this_module_serves() {
        // The page and the route table are edited independently; if they drift, the
        // console renders its error state on a healthy machine and the symptom points at
        // the daemon rather than at the mismatch.
        assert!(PAGE.contains("\"/api/provision\""));
        for (method, path) in [("POST", "/api/provision"), ("GET", "/api/provision")] {
            let request = Request {
                method: method.to_owned(),
                path: path.to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            };
            // Not 404: whether it is allowed is http::route's business, but the route
            // has to exist here.
            let job = std::sync::Arc::new(crate::provision::Job::new());
            if method == "POST" {
                assert!(job.begin(), "claimed so the POST does not start a download");
            }
            let response = respond(
                &request,
                &Fixture::new(),
                &job,
                &std::sync::Arc::new(crate::plex::Handle::new()),
                &std::sync::Arc::new(crate::update::Job::new()),
            );
            assert_ne!(response.status, 404, "{method} {path}");
        }
    }

    #[test]
    fn the_only_absolute_url_on_the_page_points_at_this_machine() {
        // The blanket "no http:// anywhere" rule this replaces was right about assets
        // and wrong about the one link the page has to offer: Plex's own interface, on
        // port 32400 of the appliance itself. It cannot be relative -- it is a different
        // port -- and it cannot use location.protocol, because Plex serves plain HTTP
        // whatever the console ends up served over.
        //
        // So the rule becomes sharper rather than looser: every absolute URL must be
        // built from the host the page was served from. A CDN or a font still fails,
        // which is what the original test existed to catch.
        let occurrences = PAGE.match_indices("http://").count();
        assert_eq!(occurrences, 1, "exactly one absolute URL is expected");

        let (index, _) = PAGE.match_indices("http://").next().expect("one");
        let after = &PAGE[index + "http://".len()..];
        assert!(
            after.starts_with("${esc(location.hostname)}"),
            "an absolute URL must be built from this machine's own address, but the page \
             has: {}",
            &after[..after.len().min(60)]
        );
    }

    #[test]
    fn the_status_route_returns_parsable_json() {
        let response = respond_test(&get("/api/status"), &Fixture::new());
        assert_eq!(response.status, 200);
        let parsed: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert!(parsed.get("gpu").is_some());
    }

    #[test]
    fn the_gpu_route_returns_the_report_alone() {
        let response = respond_test(&get("/api/gpu"), &Fixture::new());
        let parsed: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert!(
            parsed.get("health").is_some() && parsed.get("findings").is_some(),
            "the report itself, not the wrapper: {parsed}"
        );
    }

    /// Runs `claim` against a fresh path, returning the credential and everything logged.
    fn claim_into(name: &str) -> (crate::auth::Credential, Vec<String>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        let mut lines = Vec::new();
        let credential = claim(&path, &mut |line| lines.push(line.to_owned()));
        (credential, lines, path)
    }

    /// The token out of a banner, in the form a person would type back, if one was
    /// printed.
    ///
    /// Matched by normalising each line rather than by looking for an exact format, so
    /// this keeps working if the banner's spacing changes and stops working if the token
    /// itself does.
    fn token_in(lines: &[String]) -> Option<String> {
        lines.iter().find_map(|line| {
            let candidate = crate::auth::normalise(line);
            (candidate.len() == crate::auth::TOKEN_CHARS
                && line.chars().all(|c| !c.is_ascii_lowercase()))
            .then_some(candidate)
        })
    }

    #[test]
    fn the_progress_route_answers_before_anything_has_been_installed() {
        // The page polls this from the moment it loads, including on a machine nobody
        // has ever provisioned. Answering 404 there would have the console render its
        // error state on a perfectly good appliance.
        let response = respond_test(&get("/api/provision"), &Fixture::new());
        assert_eq!(response.status, 200);
        let parsed: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(parsed["phase"], "idle");
    }

    #[test]
    fn a_second_installation_is_refused_while_one_is_running() {
        // Two runs would unpack into the same staging directory and produce an image
        // neither could vouch for. Refused here rather than by the page, because the
        // page is not the only thing that can send a POST.
        let job = std::sync::Arc::new(crate::provision::Job::new());
        assert!(
            job.begin(),
            "something is already running before the test started"
        );

        let request = Request {
            method: "POST".to_owned(),
            path: "/api/provision".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let response = respond(
            &request,
            &Fixture::new(),
            &job,
            &std::sync::Arc::new(crate::plex::Handle::new()),
            &std::sync::Arc::new(crate::update::Job::new()),
        );
        assert_eq!(response.status, 409);
        assert!(
            String::from_utf8_lossy(&response.body).contains("GET /api/provision"),
            "and says where to watch the one that is running"
        );
    }

    // The success path of POST /api/provision is deliberately not exercised here: it
    // spawns a thread that downloads 83 MB from Plex, which is not a unit test. What it
    // depends on is covered where it can be — provision::Job's exclusivity and its
    // outcome handling in that module's tests, and the whole pipeline by the
    // `provision` example against real packages -- and, since then, by the appliance
    // itself running the whole path from a browser.

    #[test]
    fn a_device_with_no_credential_claims_itself_and_shows_the_token() {
        // The whole of ADR-0013's first step. Until this ran, auth:: could generate and
        // store a token and nothing called it, so Credential::Unset was permanent and
        // every mutating route answered 503 for ever.
        let (credential, lines, path) = claim_into("plexos-claim-fresh");

        let token = token_in(&lines).expect("the token is printed on the attached console");
        let crate::auth::Credential::Set(fingerprint) = credential else {
            panic!("claiming must leave the device claimed");
        };
        assert!(
            crate::auth::matches(&token, &fingerprint),
            "the token shown is the one the console will accept"
        );
        assert_eq!(
            crate::auth::read(&path),
            crate::auth::Credential::Set(fingerprint),
            "and it survives a restart"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_token_itself_is_never_written_to_disk() {
        // Only the fingerprint is stored, which is what makes a pulled disk or a backup
        // of /var useless to whoever has it.
        let (_, lines, path) = claim_into("plexos-claim-not-stored");
        let token = token_in(&lines).expect("a token");
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(
            !stored.contains(&token),
            "the file holds the digest, not the token"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_already_claimed_device_is_not_reissued_a_token() {
        // Reclaiming on every start would invalidate the token the administrator wrote
        // down, and would do it silently at the next power cut.
        let (first, _, path) = claim_into("plexos-claim-twice");
        let mut lines = Vec::new();
        let second = claim(&path, &mut |line| lines.push(line.to_owned()));

        assert_eq!(first, second, "the same credential");
        assert_eq!(
            token_in(&lines),
            None,
            "and no second token printed: {lines:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_credential_that_cannot_be_stored_leaves_the_device_unclaimed() {
        // Fail closed. Carrying on with a token that was never written would give a
        // device that accepts a credential now and rejects it after a reboot, which is
        // harder to diagnose than one that plainly cannot be claimed.
        let unwritable = std::path::Path::new("/proc/plexos-claim-cannot-exist/token");
        let mut lines = Vec::new();
        let credential = claim(unwritable, &mut |line| lines.push(line.to_owned()));

        assert_eq!(credential, crate::auth::Credential::Unset);
        let logged = lines.join("\n");
        assert!(
            logged.contains("/var is mounted") || logged.contains("could not store"),
            "and names a remedy: {logged}"
        );
    }

    #[test]
    fn an_unknown_path_lists_the_ones_that_exist() {
        // Every diagnostic names a remedy, including a 404.
        let response = respond_test(&get("/dashboard"), &Fixture::new());
        assert_eq!(response.status, 404);
        let body = String::from_utf8(response.body).unwrap();
        assert!(
            body.contains("/api/status"),
            "names what does exist: {body}"
        );
    }
}
