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

/// The port the console is served on.
///
/// TLS only. ADR-0014 deferred this while the update path was unsigned, on the grounds
/// that closing the smaller opening first would imply a guarantee the system did not
/// provide. ADR-0006 is finished and proven, so the guarantee is now real and the root
/// shell no longer travels in clear.
pub const HTTPS_PORT: u16 = 443;

/// The port that redirects to it, and serves nothing else.
pub const HTTP_PORT: u16 = 80;

/// Default port: HTTPS, because that is the only thing this console serves (ADR-0014).
pub const DEFAULT_PORT: u16 = HTTPS_PORT;

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
    install: &std::sync::Arc<crate::install::Job>,
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

        // Putting PlexOS on a disk (ADR-0016). The most destructive route here, and the
        // only one whose refusals are the point rather than the edge cases.
        ("GET" | "HEAD", "/api/install") => report_disks(env, install),

        ("POST", "/api/install") => begin_install(request, env, install),

        // Network shares: the library lives on a NAS, and without one there is nothing
        // to play.
        ("GET" | "HEAD", "/api/shares") => match serde_json::to_string(&crate::shares::states()) {
            Ok(json) => Response::json(json),
            Err(error) => Response::text(500, format!("could not serialise shares: {error}\n")),
        },

        ("POST", "/api/shares") => crate::shares::handle(&request.body),

        // The configuration, what the machine is doing about it, and confirming an
        // address change. Grouped into one helper because `respond` is a route table and
        // these three share everything except their verb.
        ("GET" | "HEAD" | "POST", "/api/config" | "/api/network")
            if request.path != "/api/network" || request.method == "POST" =>
        {
            config_route(&request.method, &request.path, &request.body)
        }

        // The terminal (ADR-0014). Every route here is a POST, including the one that
        // only reads output — deliberately, because http::route gates on the method and
        // a root shell's output must not be readable without the token. This is the one
        // place in the console where "GET is safe" is false.
        ("POST", "/api/terminal") => terminal_route(&request.body),

        ("POST", "/api/token") => rotate_token(),

        // The three questions above the interface list: a resolver, a route, and a name
        // that actually resolves. Its own route rather than a field on /api/status
        // because the name lookup blocks for seconds, and the status page polls -- the
        // one view that must keep answering while the machine is unwell must not wait on
        // the machine being well.
        //
        // Unauthenticated, like every other GET here. It reveals the nameservers and the
        // gateway, which is less than the interface list already on /api/status, and the
        // host it looks up is a constant rather than anything a caller chooses.
        ("GET" | "HEAD", "/api/network") => {
            match serde_json::to_string_pretty(&crate::netdiag::gather()) {
                Ok(json) => Response::json(json),
                Err(error) => Response::text(
                    500,
                    format!("could not serialise the network diagnosis: {error}\n"),
                ),
            }
        }

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

/// Issues a new device token and puts it in force at once.
///
/// Rotating used to mean deleting a file from a shell on the attached screen and
/// rebooting — one of the last operations that still needed the panel, and the one
/// ADR-0014 made most awkward by turning the token into a root shell.
///
/// The new token is in the reply and nowhere else. The file holds a fingerprint, and
/// there is deliberately no route that will say it again: a credential a machine can be
/// asked to repeat is one an attacker can ask it to repeat.
fn rotate_token() -> Response {
    match crate::auth::rotate(std::path::Path::new(crate::auth::CREDENTIAL_FILE)) {
        Ok(token) => {
            // Also to the attached screen, which is where ADR-0013 says a device
            // announces itself — and the only place left if the browser that asked for
            // it loses the reply.
            let shown = crate::auth::grouped(&token);
            println!("plexosd: the device token is now {shown}");
            Response::json(format!("{{\"token\":\"{shown}\"}}"))
        }
        Err(error) => Response::text(
            500,
            format!(
                "could not issue a new token: {error}. Remedy: the old one still works — \
                 nothing changes until both the file and the running server have the new \
                 one.\n"
            ),
        ),
    }
}

/// The settings routes: read, write, and confirm an address change.
///
/// Together because they are one subject and `respond` is a table. Reading is open like
/// every other `GET` here; writing and confirming are `POST`, so [`crate::http::route`]
/// has already demanded the token before this is reached.
///
/// The confirmation carries no body and needs none. Reaching this route at all is the
/// proof being asked for: the console is still reachable at the new address.
fn config_route(method: &str, path: &str, body: &[u8]) -> Response {
    if path == "/api/network" {
        let confirmed = crate::addressing::confirm();
        return Response::json(format!("{{\"confirmed\":{confirmed}}}"));
    }

    if method == "POST" {
        return match save_settings(body) {
            Ok(json) => Response::json(json),
            Err(error) => Response::text(400, format!("{error}\n")),
        };
    }

    match serde_json::to_string_pretty(&crate::settings::view(&crate::settings::path())) {
        Ok(json) => Response::json(json),
        Err(error) => Response::text(500, format!("could not serialise the settings: {error}\n")),
    }
}

/// The terminal's four actions, behind one `POST` route.
///
/// One route rather than four paths because all four must be `POST`: [`crate::http`]
/// gates on the method, and reading a root shell's output is not a safe operation however
/// much it looks like a read. A `GET /api/terminal/output` would have been ungated, which
/// is the kind of mistake that is obvious once written down and invisible while writing
/// it.
///
/// Every call first expires an idle session. There is no timer thread: a session nobody
/// is polling is one nobody will notice the closing of, so the next request of any kind
/// does the work.
fn terminal_route(body: &[u8]) -> Response {
    crate::terminal::expire_if_idle();

    let request: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => return Response::text(400, format!("the body is not JSON: {error}\n")),
    };

    let id = request.get("id").and_then(serde_json::Value::as_str);
    let size = plexos_sys::pty::WindowSize {
        rows: u16::try_from(
            request
                .get("rows")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(0),
        columns: u16::try_from(
            request
                .get("columns")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(0),
    };
    // A browser that sends nothing sensible gets the size every program understands
    // rather than 0x0, which some draw nothing at all into.
    let size = if size.rows == 0 || size.columns == 0 {
        plexos_sys::pty::WindowSize::default()
    } else {
        size
    };

    let result = match request.get("action").and_then(serde_json::Value::as_str) {
        Some("open") => {
            let take_over = request
                .get("take_over")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            crate::terminal::open(size, take_over).and_then(|opened| json(&opened))
        }
        Some("poll") => {
            let since = request
                .get("since")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            id.ok_or_else(|| "poll needs an id".to_owned())
                .and_then(|id| crate::terminal::poll(id, since))
                .and_then(|output| json(&output))
        }
        Some("input") => {
            let data = request
                .get("data")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            id.ok_or_else(|| "input needs an id".to_owned())
                .and_then(|id| crate::terminal::input(id, data.as_bytes()))
                .map(|()| "{}".to_owned())
        }
        Some("resize") => id
            .ok_or_else(|| "resize needs an id".to_owned())
            .and_then(|id| crate::terminal::resize(id, size))
            .map(|()| "{}".to_owned()),
        Some("close") => {
            if let Some(id) = id {
                crate::terminal::close(id);
            }
            Ok("{}".to_owned())
        }
        other => Err(format!(
            "{other:?} is not a terminal action. Remedy: one of open, poll, input, \
             resize, close."
        )),
    };

    match result {
        Ok(json) => Response::json(json),
        // 409 rather than 400 for a session conflict: the request was well formed and the
        // machine's state is what refused it, and the page offers "take over" on exactly
        // that answer.
        Err(error) if error.contains("already open") => Response::text(409, format!("{error}\n")),
        Err(error) => Response::text(400, format!("{error}\n")),
    }
}

/// Serialises a value, turning a failure into the same `String` error the routes use.
fn json<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| format!("could not serialise the reply: {e}"))
}

/// Applies a settings patch and reports what the machine did about it.
///
/// The response carries the whole [`crate::settings::View`] as well as the per-field
/// outcomes, so the page can re-render from one answer rather than saving and then
/// polling to find out what happened — and so that "stored but not in force" is visible
/// in the same breath as the save.
fn save_settings(body: &[u8]) -> Result<String, String> {
    let path = crate::settings::path();

    // Loaded, patched, stored: the body carries only the fields the page edits, so a
    // newer page adding a field cannot have it reverted by an older one.
    let previous = crate::settings::load(&path)?;
    let mut config = previous.clone();
    crate::settings::patch(&mut config, body)?;
    let touched_network = crate::settings::patch_network(&mut config, body)?;

    let mut log = |line: &str| println!("plexosd: addressing: {line}");
    let applied = if touched_network {
        crate::settings::store_with_network(&config, &previous, &path, &mut log)?
    } else {
        crate::settings::store(&config, &path)?
    };

    serde_json::to_string_pretty(&serde_json::json!({
        "applied": applied,
        "view": crate::settings::view(&path),
    }))
    .map_err(|e| format!("could not serialise the result: {e}"))
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
    // First, and before the network. The hostname travels in the DHCP request, so
    // applying it afterwards would announce the machine under one name while it held
    // another until the lease renewed -- and the name in the router's client list is
    // exactly how somebody finds an appliance with no keyboard.
    //
    // On every boot rather than only when something changes it: the kernel forgets its
    // hostname at reboot, so a name set once and never re-applied lasts until the power
    // goes off.
    match crate::settings::load(&crate::settings::path()) {
        Ok(config) => {
            let applied = crate::settings::apply(&config);
            log(&format!("hostname: {:?}", applied.hostname));
            log(&format!("timezone: {:?}", applied.timezone));
        }
        Err(error) => log(&format!(
            "could not read the configuration: {error}. The machine runs with kernel \
             defaults, and the settings page says the same thing rather than showing \
             defaults as though they were somebody's choices."
        )),
    }

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
    let listener = bind(address)?;
    log(&format!("console listening on {address}"));

    // Not fatal, and deliberately not the console. It exists so that `http://<address>/`
    // -- what a person types, and what every note in this repository has told them to
    // type -- lands on a redirect instead of a refused connection. A console on a high
    // port is somebody testing, and taking port 80 as well is not something they asked
    // for.
    let cleartext = if port == HTTPS_PORT {
        match bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, HTTP_PORT))) {
            Ok(listener) => Some(listener),
            Err(error) => {
                log(&format!(
                    "nothing will redirect http:// to https://: {error}"
                ));
                None
            }
        }
    } else {
        None
    };

    let addresses = wait_for_addresses(configured, port, log);

    // The certificate is issued here rather than before the wait, so that it names the
    // address somebody is about to type. The socket is already bound, so a browser that
    // arrives during this waits in the backlog instead of being refused.
    let tls = identity_for(&addresses, log)?;

    if let Some(cleartext) = cleartext {
        std::thread::spawn(move || {
            let mut log = |line: &str| println!("plexosd: {line}");
            let _ = http::serve_redirect(&cleartext, &mut log);
        });
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

    // Started after the first attempt above, so that the supervisor's first look sees
    // either a live child or a machine with nothing installed, rather than racing the
    // start it would otherwise duplicate.
    //
    // On a thread of its own and not folded into the gate's: the gate runs once and this
    // runs for ever, and a supervisor that stopped when the gate finished would keep Plex
    // alive for exactly as long as nobody needed it to.
    let watched = std::sync::Arc::clone(&plex);
    std::thread::spawn(move || {
        let mut log = |line: &str| println!("plexosd: plex: {line}");
        crate::plex::supervise(
            &watched,
            std::path::Path::new(plexos_types::paths::PLEX_MOUNT),
            &mut log,
        )
    });

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
    let installer = std::sync::Arc::new(crate::install::Job::new());
    http::serve_tls(
        &listener,
        &tls,
        credential,
        move |request| respond(request, &System, &served_job, &plex, &update, &installer),
        log,
    )
}

/// Binds a listener, with a remedy that matches the reason it failed.
fn bind(address: SocketAddr) -> io::Result<TcpListener> {
    let port = address.port();
    TcpListener::bind(address).map_err(|error| {
        // The remedy has to match the cause. "Try a higher port" is good advice for
        // EACCES and actively misleading for EADDRINUSE, where the port is fine and
        // something else is holding it.
        let remedy = match error.kind() {
            io::ErrorKind::PermissionDenied => {
                "Ports below 1024 need root, and this is not running as root. Either                  start it as root or pass --port with a number above 1024."
                    .to_owned()
            }
            io::ErrorKind::AddrInUse => format!(
                "Something is already listening on port {port}. Find it with                  `netstat -tlnp | grep {port}`, or pass --port with a free one."
            ),
            _ => "Check that the address is one this machine can bind.".to_owned(),
        };
        io::Error::new(
            error.kind(),
            format!("could not bind {address}: {error}. {remedy}"),
        )
    })
}

/// The disks on this machine, with the one PlexOS runs from marked.
fn report_disks(env: &impl Environment, install: &std::sync::Arc<crate::install::Job>) -> Response {
    let source = crate::install::running_disk(env);
    match serde_json::to_string(&install.snapshot(env, source.as_deref())) {
        Ok(json) => Response::json(json),
        Err(error) => Response::text(500, format!("could not serialise the disks: {error}\n")),
    }
}

/// Vets an install request and, if it survives, hands it to a thread.
///
/// Split out of [`respond`] because every line of it is a refusal, and a route table is
/// the wrong place to read them: the decision about erasing somebody's disk should be one
/// function somebody can look at whole.
fn begin_install(
    request: &Request,
    env: &impl Environment,
    install: &std::sync::Arc<crate::install::Job>,
) -> Response {
    let (disk, confirm) = crate::install::request_in(&request.body);

    // Refused outright when the running disk cannot be identified, rather than proceeding
    // with nothing excluded. "I do not know which disk I am running from" and "no disk is
    // excluded" are the same value and opposite meanings, and taking the second one erases
    // the running system.
    let Some(source_disk) = crate::install::running_disk(env) else {
        return Response::text(
            500,
            "this machine's own disk could not be identified, so no disk can safely be \
             erased. Remedy: none from the console -- PlexOS finds its own disk behind the \
             verified /usr, and not finding it means this is not a booted PlexOS system.\n",
        );
    };

    let disks = match crate::install::candidates(env, Some(&source_disk)) {
        Ok(disks) => disks,
        Err(error) => return Response::text(500, format!("could not read the disks: {error}\n")),
    };

    // Vetted before the job is claimed, so a refused request leaves the console exactly as
    // it found it rather than in a state somebody has to clear.
    let target = match crate::install::vet(&disks, &disk, &confirm) {
        Ok(target) => target.clone(),
        Err(refusal) => return Response::text(400, format!("{refusal}\n")),
    };

    let source = match crate::install::Source::resolve(crate::update::running_slot()) {
        Ok(source) => source,
        Err(error) => {
            return Response::text(
                500,
                format!(
                    "this system's own partitions could not be found ({error}), so there \
                     is nothing to copy. This is not a PlexOS disk.\n"
                ),
            );
        }
    };

    if !install.begin() {
        return Response::text(
            409,
            "An install is already running. Watch it at GET /api/install.\n",
        );
    }
    crate::install::spawn(install, target, source);
    Response::json(format!("{{\"disk\":\"{disk}\"}}"))
}

/// Waits for the interface to get an address, and reports where the console is.
///
/// After binding, so the socket exists as early as possible, and after `configure` rather
/// than inside it, because `udhcpc` is spawned and never waited on. Printing the URL
/// without this waiting step prints it before any lease can exist, which is to say never
/// — the console worked and said nothing a person could act on.
fn wait_for_addresses(
    configured: Option<net::Interface>,
    port: u16,
    log: &mut dyn FnMut(&str),
) -> Vec<String> {
    let mut addresses = Vec::new();
    let Some(interface) = configured else {
        return addresses;
    };

    match net::wait_for_address(&System, &interface.name, net::LEASE_TIMEOUT, log) {
        Some(found) => {
            addresses.push(found.ip().to_string());
            log(&format!("console at https://{}/", found.ip()));
        }
        None => log(&format!(
            "{} is up but DHCP produced no address in {}s. The console is serving on \
             port {port} and unreachable until the interface has one. Check for a DHCP \
             server on this segment, or set an address by hand with \
             `ip addr add <a.b.c.d/nn> dev {}`.",
            interface.name,
            net::LEASE_TIMEOUT.as_secs(),
            interface.name
        )),
    }
    addresses
}

/// Issues or reloads the console's certificate and reports what a person has to check.
fn identity_for(
    addresses: &[String],
    log: &mut dyn FnMut(&str),
) -> io::Result<std::sync::Arc<rustls::ServerConfig>> {
    let identity = crate::tls::load_or_create(
        std::path::Path::new(plexos_types::paths::TLS_DIR),
        &crate::tls::names_for(addresses, &hostname()),
    )?;
    crate::tls::remember(&identity.fingerprint);

    // On the attached screen, which is the only place a fingerprint can be compared
    // against what a browser shows before the first connection. ADR-0014 called that
    // tension real and unresolved and it still is: this console exists so nobody needs
    // the screen, and the one check that makes a self-signed certificate mean anything
    // can happen nowhere else.
    log(&format!(
        "certificate fingerprint SHA256 {}",
        identity.fingerprint
    ));
    if identity.key_is_new {
        log(
            "this is a new key, so a browser that trusted this console before will warn \
             again. That is correct and worth reading: nothing else should have changed \
             it.",
        );
    }

    crate::tls::server_config(&identity)
}

/// This machine's host name, for the certificate.
///
/// Read from the kernel rather than from the configuration file: the configuration says
/// what it should be and this says what it is, and a certificate has to name what a
/// browser will actually be sent to.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_owned()
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
            &std::sync::Arc::new(crate::install::Job::new()),
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
                &std::sync::Arc::new(crate::install::Job::new()),
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
    fn the_pages_script_parses() {
        // The console is one inline script, so **a syntax error anywhere in it stops the
        // whole page**: every section stays on "Loading..." and the machine looks dead
        // while its API answers perfectly. That is exactly what shipped once, from a
        // second `const signature` in a function that already had one -- a duplicate
        // `const` is a parse error, not a runtime one, so nothing rendered at all.
        //
        // Nothing else in this repository ever parsed this file. The tests below assert
        // that strings are present in it, which a broken script satisfies completely.
        //
        // Skipped rather than failed when no JavaScript engine is installed: this must not
        // make the suite unrunnable on a host that has no reason to carry one. A skip is
        // announced, because a check nobody knows was skipped is a check nobody has.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has exactly one script block");

        let engine = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        });
        let Some(engine) = engine else {
            println!(
                "skip: the console script was not parsed -- no node, deno or qjs on this \
                 host. Install one, or a syntax error in the page will only be found by \
                 loading it in a browser."
            );
            return;
        };

        let file = std::env::temp_dir().join("plexos-console-check.js");
        std::fs::write(&file, script).expect("scratch is writable");
        let checked = std::process::Command::new(engine)
            .arg("--check")
            .arg(&file)
            .output()
            .expect("the engine runs");
        let _ = std::fs::remove_file(&file);

        assert!(
            checked.status.success(),
            "the console's script does not parse, so no section of the page will render:\n{}",
            String::from_utf8_lossy(&checked.stderr)
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
            PAGE.contains("Signed by"),
            "the page must name what vouched for an update, not merely that something did"
        );
        assert!(
            PAGE.contains("development root key"),
            "and must say when that chain ends in a key whose private half is on a build \
             host, because 'signed' alone would tell the reader something false"
        );
        assert!(
            PAGE.contains("comes back to this one by itself"),
            "and must say what happens when an update is bad, which is the question \
             anybody hesitating over that button is actually asking"
        );
    }

    #[test]
    fn the_page_says_what_installing_destroys_before_it_offers_to_do_it() {
        // The only control on this page that erases data which was never PlexOS's. The
        // warning is part of the markup rather than something a render function might skip
        // in some state, and the confirmation is a text field because a checkbox is a
        // thing people tick.
        assert!(PAGE.contains("\"/api/install\""));
        assert!(
            PAGE.contains("This erases the disk you choose, completely."),
            "the destruction has to be stated before the button, not after"
        );
        assert!(
            PAGE.contains("Type the name of the disk to confirm"),
            "a typed confirmation, not a tick"
        );
        assert!(
            PAGE.contains("not offered"),
            "and it must say that the disk it is running from is excluded"
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
                &std::sync::Arc::new(crate::install::Job::new()),
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

    #[test]
    fn the_config_route_reports_the_machine_beside_the_file() {
        // Both, not one. A hostname stored and never applied is a real state, and a page
        // shown only the file would render it as though it had taken effect -- which is
        // the failure this whole settings path was written to refuse.
        let response = respond_test(&get("/api/config"), &Fixture::new());
        let parsed: serde_json::Value = serde_json::from_slice(&response.body).unwrap();

        assert!(parsed.get("config").is_some(), "{parsed}");
        assert!(
            parsed.get("hostname_now").is_some(),
            "what the kernel is actually using: {parsed}"
        );
        assert!(
            parsed.get("timezones").is_some(),
            "the zones this image can be set to, so the field offers nothing rather \
             than names that would be refused: {parsed}"
        );
    }

    #[test]
    fn a_settings_patch_leaves_untouched_fields_alone() {
        // The body carries only what the page edits. A replacement would mean an older
        // page silently reverting a field a newer one had added, which is a data-loss
        // bug that looks like a successful save.
        let mut config = plexos_types::config::Config::default();
        config.system.timezone = "Europe/Warsaw".to_owned();

        crate::settings::patch(&mut config, br#"{"system":{"hostname":"cinema"}}"#)
            .expect("a valid patch");

        assert_eq!(config.system.hostname, "cinema");
        assert_eq!(
            config.system.timezone, "Europe/Warsaw",
            "a field the body did not mention must survive"
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
            &std::sync::Arc::new(crate::install::Job::new()),
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
