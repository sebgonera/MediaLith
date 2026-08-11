//! The status console: the routes, and the page.
//!
//! This is the first thing in MediaLith a person interacts with rather than reads off a
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

/// The daemon's shared state, as one thing the route table can be handed.
///
/// This was seven separate parameters until the activity card needed an eighth, at which
/// point clippy's argument limit said what the signature had been saying for a while: a
/// route table needs whatever the daemon holds, that set grows with every feature, and
/// threading it positionally is how two `Arc<Job>`s of different types eventually get
/// passed the wrong way round. Named fields cost nothing and the compiler checks them.
///
/// Every field is an `Arc` because the work behind these routes outlives the request that
/// started it: an install is minutes, a provision is longer, and the sampler has to keep
/// the previous reading or no percentage on the dashboard can exist.
pub struct Services {
    /// The state of any Plex provisioning run.
    ///
    /// Reached from every request because `GET /api/provision` is how the page follows one,
    /// and a request that started an installation returns long before it finishes.
    pub provision: std::sync::Arc<crate::provision::Job>,
    /// The Plex process itself.
    pub plex: std::sync::Arc<crate::plex::Handle>,
    /// The state of any OS update.
    pub update: std::sync::Arc<crate::update::Job>,
    /// The state of any install-to-disk.
    pub install: std::sync::Arc<crate::install::Job>,
    /// The state of any wireless scan or join.
    pub wifi: std::sync::Arc<crate::wifi::Job>,
    /// The previous reading of `/proc`, which is what makes a rate possible.
    pub metrics: std::sync::Arc<crate::metrics::Sampler>,
}

impl Services {
    /// A daemon that has done nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            provision: std::sync::Arc::new(crate::provision::Job::new()),
            plex: std::sync::Arc::new(crate::plex::Handle::new()),
            update: std::sync::Arc::new(crate::update::Job::new()),
            install: std::sync::Arc::new(crate::install::Job::new()),
            wifi: std::sync::Arc::new(crate::wifi::Job::new()),
            metrics: std::sync::Arc::new(crate::metrics::Sampler::new()),
        }
    }
}

impl Default for Services {
    fn default() -> Self {
        Self::new()
    }
}

/// Answers one request, against the machine described by `env`.
///
/// Separated from the socket so the whole route table can be tested against a recorded
/// machine, which is the same boundary every other module here draws.
///
/// This function does **not** check the device token. [`http::route`] does, before this
/// is called, for every method that is not a read — so a route added here is
/// authenticated by construction rather than by its author remembering to be.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "this is a dispatch table, and its length is the number of routes rather than \
              complexity. Splitting it to satisfy a line count would scatter the one thing \
              whose value is being in a single place -- every route the console answers, \
              readable in order, next to the comment saying why each is a GET or a POST. \
              `expect` rather than `allow` so that a future version short enough not to \
              need this is told to drop it."
)]
pub fn respond(request: &Request, env: &impl Environment, services: &Services) -> Response {
    let Services {
        provision: job,
        plex,
        update,
        install,
        wifi,
        metrics,
    } = services;

    match (request.method.as_str(), request.path.as_str()) {
        ("GET" | "HEAD", "/api/metrics") | ("POST", "/api/metrics/processes") => {
            metrics_route(request, env, metrics)
        }

        // What Plex is playing right now. A `POST` for the same reason the process list is
        // one, and it is the stronger case of the two: a title, a username, a device name
        // and a position in a film are what somebody in this house is doing this evening,
        // and a `GET` would make a household's viewing readable by anything on the LAN for
        // as long as the appliance runs. The method-based gate in `http::refusal` is what
        // enforces it — one policy, in front of the whole table, rather than a check in
        // here that a later route could forget.
        //
        // The open-`GET` principle is untouched: it exists so a *broken* machine can still
        // be diagnosed, and nothing here is needed to diagnose one. `/api/status`,
        // `/api/gpu` and `/healthz` answer as freely as they ever did.
        ("POST", "/api/plex/sessions") => {
            let report =
                crate::plexactivity::observe(std::path::Path::new(plexos_types::paths::PLEX_MOUNT));
            match serde_json::to_string(&report) {
                Ok(body) => Response::json(body),
                Err(error) => Response::text(
                    500,
                    format!("could not serialise the Plex activity: {error}\n"),
                ),
            }
        }
        // Starting an installation. Returns as soon as the work is handed to a thread:
        // the download alone is minutes, and a request held open for it would time out
        // in the browser with the install still running and no way to say so.
        ("POST", "/api/provision") => start_download(job, plex),

        // Asking Plex what it publishes, and installing nothing. A POST because it reaches
        // the network on the appliance's behalf, which is a thing a token should gate even
        // though it changes no state here.
        ("POST", "/api/provision/check") => {
            crate::provision::spawn_check(job);
            Response::json("{\"checking\":true}".to_owned())
        }

        // ADR-0010's removable media, the half a browser cannot do. A scan is a GET
        // because it changes nothing -- it mounts read-only, looks, and unmounts -- and
        // because somebody diagnosing "why does it not see my stick" should not need a
        // token to ask.
        ("GET", "/api/media") => {
            let running = crate::install::running_disk(env);
            match serde_json::to_string(&crate::media::scan(env, running.as_deref())) {
                Ok(body) => Response::json(body),
                Err(error) => {
                    Response::text(500, format!("could not describe the media: {error}\n"))
                }
            }
        }

        ("POST", "/api/provision/media") => start_from_media(request, env, job, plex),

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

        // What a new appliance still needs, in order (ADR-0016). Read-only and derived
        // entirely from state other endpoints already report, so it cannot disagree with
        // them.
        ("GET" | "HEAD", "/api/setup") => {
            match serde_json::to_string(&crate::setup::Report::observe()) {
                Ok(json) => Response::json(json),
                Err(error) => Response::text(500, format!("could not serialise setup: {error}\n")),
            }
        }

        // Putting MediaLith on a disk (ADR-0016). The most destructive route here, and the
        // only one whose refusals are the point rather than the edge cases.
        ("GET" | "HEAD", "/api/install") => report_disks(env, install),

        ("POST", "/api/install") => begin_install(request, env, install),

        // Network shares: the library lives on a NAS, and without one there is nothing
        // to play.
        // Wireless. GET reads state and needs no credential, like every other GET here;
        // the *key* is deliberately not among what it answers, because this route is
        // readable by anyone on the LAN and the key sits on /var at 0600 for a reason.
        // Reading needs no credential and both the scan and the join hold the radio, so
        // they are POSTs; `http::route` has already enforced that by the time this runs.
        ("GET" | "HEAD" | "POST", "/api/wifi") => wifi_route(request, env, wifi),

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

        // Pairing (ADR-0019). The one mutating route that carries no credential, because
        // it is how a browser gets one -- `http::refusal` names it and lets it past. What
        // it accepts is a code that only somebody standing at the machine's own screen can
        // have caused to exist.
        ("POST", crate::http::PAIR_ROUTE) => pair_route(&request.body),

        // The session a pairing produced: is it still good, and end it.
        ("POST", "/api/session") => session_route(request),

        // One browser approving another (ADR-0019). The first three carry no credential --
        // `http::refusal` names them -- because they are how a browser obtains one; the
        // last three decide whether somebody else is let in, so the gate has already
        // demanded an administrator before they are reached.
        ("POST", crate::http::BROWSER_PAIR_START) => browser_pair_start(request),
        ("POST", crate::http::BROWSER_PAIR_REDEEM) => browser_pair_redeem(&request.body),
        ("POST", crate::http::BROWSER_PAIR_CANCEL) => browser_pair_cancel(&request.body),
        ("POST", "/api/browser-pair/waiting") => browser_pair_waiting(),
        ("POST", "/api/browser-pair/inspect") => browser_pair_inspect(&request.body),
        ("POST", "/api/browser-pair/approve") => browser_pair_decide(&request.body, true),
        ("POST", "/api/browser-pair/deny") => browser_pair_decide(&request.body, false),

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
            match serde_json::to_string(&provision_report(job, plex)) {
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
pub fn claim(
    path: &std::path::Path,
    log: &mut dyn FnMut(&str),
) -> (crate::auth::Credential, Option<String>) {
    use crate::auth::Credential;

    if let Credential::Set(fingerprint) = crate::auth::read(path) {
        log("device claimed; changes need its token");
        // No plaintext, and there is nowhere to get one. Every boot after the first takes
        // this path, which is why the second return value is an Option rather than a
        // String: "the code, if it exists right now" is the honest type, and a machine
        // that could produce it on demand would be one an attacker could ask.
        return (Credential::Set(fingerprint), None);
    }

    let token = match crate::auth::generate() {
        Ok(token) => token,
        Err(error) => {
            log(&format!(
                "could not generate a device token: {error}. Nothing may change this \
                 machine until one exists. /dev/urandom is unreadable, which means /dev \
                 is not mounted -- a larger fault than the missing token."
            ));
            return (Credential::Unset, None);
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
        return (Credential::Unset, None);
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

    // The plaintext goes back to the caller as well as to the log, so the dashboard can
    // put it on the attached screen in a form somebody can read without scrolling through
    // boot messages. It is handed over, not stored: nothing writes it, and the dashboard
    // drops it the moment a key is pressed.
    (Credential::Set(fingerprint), Some(token))
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
            // Everything the old credential admitted goes with it. Rotation is what
            // somebody does when a credential has leaked, and a browser that was paired
            // under the old one is a browser somebody may have paired with it -- leaving
            // those sessions standing would make this a password change that logs nobody
            // out. The pairing offer goes for the same reason: a QR on the screen is a
            // credential in the room, and this is somebody asking for a clean slate.
            //
            // The browser that asked for the rotation is signed out by this too, which is
            // deliberate and is what the page warns about before it asks. Anything else
            // would mean deciding that one session is special, and the only way to know
            // which is to trust the request that arrived.
            let sessions = crate::session::revoke_all();
            crate::pairing::cancel();

            // Also to the attached screen, which is where ADR-0013 says a device
            // announces itself — and the only place left if the browser that asked for
            // it loses the reply.
            let shown = crate::auth::grouped(&token);
            println!("plexosd: the recovery device code is now {shown}");
            if sessions > 0 {
                println!(
                    "plexosd: {sessions} administrator browser session(s) revoked by the \
                     rotation"
                );
            }
            Response::json(format!(
                "{{\"token\":\"{shown}\",\"sessions_revoked\":{sessions}}}"
            ))
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

/// Spends a pairing code and issues an administrator session (ADR-0019).
///
/// The only route on this console that changes the machine without a credential, and the
/// only one that can be: it is where a credential comes from. What stands in for
/// authentication is that [`crate::pairing`] has nothing to spend unless somebody pressed
/// a key on the screen attached to the machine — an appliance nobody has touched answers
/// every request here identically, and says so.
///
/// # Nothing here may say the code out loud
///
/// Not in the log, not in an error message, not in a panic. The refusals come from
/// [`crate::pairing::Refusal`], which is an enum of three states and carries no input, so
/// the code has no path into a message even by accident. The success line names the address
/// that paired and nothing else.
///
/// # Bounded before it is parsed
///
/// `http` already caps a body at [`crate::http::MAX_BODY`]; this caps the field as well,
/// because a 64 KiB "code" is not a mistyped one and there is no reason to hash it.
fn pair_route(body: &[u8]) -> Response {
    /// Longest string that could be a pairing code.
    ///
    /// The code is [`crate::pairing::SECRET_CHARS`]; the slack is for a client that sends
    /// it with punctuation rather than for anything this issues.
    const MAX_CODE: usize = 64;

    let request: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        // The parser's message describes the document's shape and never its contents, so
        // this cannot leak a code that arrived inside a malformed body.
        Err(_) => {
            return Response::text(
                400,
                "the body must be JSON of the form {\"code\": \"...\"}.\n",
            );
        }
    };

    let Some(code) = request.get("code").and_then(serde_json::Value::as_str) else {
        return Response::text(400, "the body must carry a `code` field.\n");
    };

    if code.len() > MAX_CODE {
        // Refused as wrong rather than as too long: telling a caller which of their inputs
        // was the wrong *shape* is a way to learn the shape.
        return Response::text(403, format!("{}\n", crate::pairing::Refusal::Wrong));
    }

    if let Err(refusal) = crate::pairing::consume(code) {
        return Response::text(403, format!("{refusal}\n"));
    }

    match crate::session::issue() {
        Ok(token) => {
            println!("plexosd: administrator browser paired");
            Response::json(format!(
                "{{\"authenticated\":true,\"session_token\":\"{token}\",\
                 \"expires_in\":{},\"idle_timeout\":{}}}",
                crate::session::ABSOLUTE_LIFETIME.as_secs(),
                crate::session::IDLE_TIMEOUT.as_secs()
            ))
        }
        // The code is already spent at this point and deliberately not put back: a failure
        // to read /dev/urandom is not a state to recover a credential into, and pressing P
        // again costs one keystroke.
        Err(error) => Response::text(
            500,
            format!(
                "the pairing code was accepted but no session could be issued: {error}. \
                 Remedy: press P on the screen attached to the machine to pair again. If \
                 that keeps failing, /dev is not mounted, which is a larger fault than \
                 this one.\n"
            ),
        ),
    }
}

/// Asks after the administrator session this request carries, or ends it.
///
/// `check` needs no implementation at all, which is the point of it: this route is a
/// `POST`, so [`crate::http::refusal`] has already decided whether the caller is an
/// administrator before the handler exists. Reaching here *is* the answer. A second
/// implementation of the question would be a second thing to keep in agreement with the
/// gate.
fn session_route(request: &Request) -> Response {
    let action = serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|value| {
            value
                .get("action")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        });

    match action.as_deref() {
        Some("check") | None => Response::json("{\"authenticated\":true}"),
        Some("sign-out") => {
            // The credential this request arrived with, which is the only session a
            // request is entitled to end. Signing out with the recovery device code
            // revokes nothing and is not an error: there is no server-side state behind it
            // to remove, and the browser drops its own copy either way.
            let presented = request
                .header("Authorization")
                .and_then(crate::auth::bearer)
                .unwrap_or_default();
            let revoked = crate::session::revoke(presented);
            if revoked {
                println!("plexosd: administrator session signed out");
            }
            Response::json(format!("{{\"signed_out\":{revoked}}}"))
        }
        Some(other) => Response::text(
            400,
            format!("{other:?} is not a session action. Remedy: one of check, sign-out.\n"),
        ),
    }
}

/// One field out of a small JSON body.
fn field(body: &[u8], name: &str) -> Option<String> {
    /// Longer than any value these routes take. A 256-bit value is 64 hex characters; the
    /// slack is for punctuation a client might send, not for anything this issues.
    const LONGEST: usize = 128;

    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get(name)?
        .as_str()
        .filter(|value| value.len() <= LONGEST)
        .map(ToOwned::to_owned)
}

/// Asks to be approved by a browser that is already an administrator (ADR-0019).
///
/// Unauthenticated, and that is safe because asking is not being let in: this creates a
/// request that does nothing at all until an authenticated administrator approves it. What
/// it costs an attacker who floods it is one of sixteen slots for five minutes, which is
/// why the store refuses rather than evicting.
///
/// The reply carries the desktop's secret, and that is the only time it exists anywhere but
/// in the browser that asked. It is deliberately **not** in the QR: the whole two-value
/// design is that photographing the screen is not enough.
fn browser_pair_start(request: &Request) -> Response {
    let agent = request.header("User-Agent").unwrap_or_default();

    let opened = match crate::browserpair::open(agent) {
        Ok(opened) => opened,
        // 503 rather than 400: nothing about the request was wrong, the appliance is simply
        // full, and the message says how long that lasts.
        Err(why) => return Response::text(503, format!("{why}\n")),
    };

    // The address is chosen the same way the physical console chooses it, through the facts
    // the whole daemon shares -- so the two QR codes on two screens never name different
    // ways to reach one machine.
    let url = format!("https://{}/#approve={}", browser_pair_host(), opened.id);

    let matrix = match crate::dashboard::qr::Symbol::encode(&url) {
        Ok(symbol) => symbol.matrix(),
        Err(why) => return Response::text(500, format!("{why}\n")),
    };

    match json(&serde_json::json!({
        "request_id": opened.id,
        "desktop_secret": opened.secret,
        "verification": opened.verification,
        "expires_in": crate::browserpair::LIFETIME.as_secs(),
        "url": url,
        "qr": matrix,
    })) {
        Ok(body) => Response::json(body),
        Err(why) => Response::text(500, format!("{why}\n")),
    }
}

/// The address to put in the desktop's QR code.
///
/// The same answer the attached screen gives, arrived at the same way: the addresses this
/// machine reports as reachable, with the ones its certificate names first. Two screens
/// naming two different ways to reach one appliance is the shape of fault that gets
/// diagnosed as "pairing does not work".
fn browser_pair_host() -> String {
    let mut addresses =
        crate::status::Status::gather_with(&System, plexos_gpu::report::Report::generate(&System))
            .network
            .reachable_at;
    crate::dashboard::model::prefer_covered(&mut addresses, crate::tls::covers);
    addresses.first().cloned().unwrap_or_default()
}

/// Everything waiting for a decision. Authenticated by the gate.
///
/// The route that makes browser approval work on a phone at all. Scanning the desktop's QR
/// code lands in a **new tab**, and a session lives in `sessionStorage`, which belongs to a
/// tab — so the browser that arrives by scanning never holds the session, in any browser,
/// on any phone. That is not a defect anywhere; it is what `sessionStorage` is.
///
/// So the signed-in browser asks instead. Nothing is scanned and nothing is pasted: the
/// desktop shows a number, this says who is asking, and the two are compared by eye.
fn browser_pair_waiting() -> Response {
    let waiting: Vec<serde_json::Value> = crate::browserpair::waiting()
        .into_iter()
        .map(|(id, described)| {
            serde_json::json!({
                "request_id": id,
                "browser": described.browser,
                "age_seconds": described.age_seconds,
                "verification": described.verification,
            })
        })
        .collect();

    match json(&serde_json::json!({ "waiting": waiting })) {
        Ok(body) => Response::json(body),
        Err(why) => Response::text(500, format!("{why}\n")),
    }
}

/// What an administrator is shown before approving. Authenticated by the gate.
fn browser_pair_inspect(body: &[u8]) -> Response {
    let Some(id) = field(body, "request_id") else {
        return Response::text(400, "the body must carry a `request_id` field.\n");
    };

    match crate::browserpair::describe(&id) {
        Some(describes) => match json(&describes) {
            Ok(body) => Response::json(body),
            Err(why) => Response::text(500, format!("{why}\n")),
        },
        None => Response::text(
            404,
            "no browser is waiting under that request. Remedy: it has either expired -- \
             they last five minutes -- or been dealt with already. Ask the other browser \
             to show a new code.\n",
        ),
    }
}

/// Approves or refuses a waiting browser. Authenticated by the gate.
///
/// **No session token crosses this route in either direction.** What an administrator sends
/// is a sentence about a request; what comes back is whether it was still there to decide.
/// The session the desktop eventually receives is minted by the appliance, when the desktop
/// redeems, out of the same store every other session comes from.
///
/// The gate accepts either credential, so a browser holding the recovery device code may
/// approve as well as one holding a session. That follows from ADR-0013 rather than being
/// decided here: the recovery code already authorises installing an operating system and
/// opening a root shell, so withholding "may let another browser in" from it would be a
/// distinction with nothing behind it.
fn browser_pair_decide(body: &[u8], approve: bool) -> Response {
    let Some(id) = field(body, "request_id") else {
        return Response::text(400, "the body must carry a `request_id` field.\n");
    };

    if crate::browserpair::decide(&id, approve) {
        println!(
            "plexosd: an administrator {} a waiting browser",
            if approve { "approved" } else { "refused" }
        );
        return Response::json(format!("{{\"approved\":{approve}}}"));
    }

    Response::text(
        409,
        "that request is no longer waiting to be decided: it has expired, or it was \
         already approved, refused or used. Remedy: ask the other browser to show a new \
         code.\n",
    )
}

/// Collects the session an approval produced.
///
/// Unauthenticated in the ordinary sense and not open: it needs the 256-bit secret returned
/// once to the browser that asked. That secret proves ownership of a request and nothing
/// else — it is not a bearer credential and no other route will look at it.
///
/// This is also the desktop's polling route, which is why "not yet" is an ordinary 200 with
/// a status in it rather than an error: a browser asking every two seconds should not be
/// reading failures while it waits.
fn browser_pair_redeem(body: &[u8]) -> Response {
    let (Some(id), Some(secret)) = (field(body, "request_id"), field(body, "desktop_secret"))
    else {
        return Response::text(
            400,
            "the body must carry `request_id` and `desktop_secret`.\n",
        );
    };

    let answer = match crate::browserpair::redeem(&id, &secret) {
        crate::browserpair::Outcome::Pending => serde_json::json!({"status": "pending"}),
        crate::browserpair::Outcome::Denied => serde_json::json!({"status": "denied"}),
        // One answer for expired, unknown and wrong-secret alike. Distinguishing them would
        // tell whoever photographed a screen that they hold half of what they need.
        crate::browserpair::Outcome::Refused => serde_json::json!({"status": "refused"}),
        crate::browserpair::Outcome::Approved(session) => {
            println!("plexosd: a browser approved by an administrator was let in");
            serde_json::json!({
                "status": "approved",
                "session_token": session,
                "expires_in": crate::session::ABSOLUTE_LIFETIME.as_secs(),
                "idle_timeout": crate::session::IDLE_TIMEOUT.as_secs(),
            })
        }
    };

    match json(&answer) {
        Ok(body) => Response::json(body),
        Err(why) => Response::text(500, format!("{why}\n")),
    }
}

/// Withdraws a request. Needs the secret, so only the browser that asked can do it.
fn browser_pair_cancel(body: &[u8]) -> Response {
    let (Some(id), Some(secret)) = (field(body, "request_id"), field(body, "desktop_secret"))
    else {
        return Response::text(
            400,
            "the body must carry `request_id` and `desktop_secret`.\n",
        );
    };

    // Whether it was there or not, the caller's desired state is the state they are in, so
    // this is not an error either way.
    let cancelled = crate::browserpair::cancel(&id, &secret);
    Response::json(format!("{{\"cancelled\":{cancelled}}}"))
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

/// What the machine is doing now, and — behind a `POST` — what is running on it.
///
/// The sampler comes from the daemon rather than being built here because a rate is the
/// difference between two readings: one created per request would have nothing to compare
/// against and would report `null` forever, which looks like an empty dashboard rather than
/// like a mistake.
///
/// The split between the two is not cosmetic. Every `GET` on this console answers without a
/// credential, deliberately, because somebody diagnosing a machine that will not boot should
/// not have to find a token first — and a list of every process with its command line is not
/// that kind of reading. It is closer to what the terminal exposes, and the terminal is all
/// `POST` for exactly this reason (ADR-0013, ADR-0014). The gate in [`http::route`] is
/// method-based, so being a `POST` *is* the protection; nothing in this function checks
/// anything.
fn metrics_route(
    request: &Request,
    env: &impl Environment,
    metrics: &crate::metrics::Sampler,
) -> Response {
    let (what, body) = if request.method == "POST" {
        (
            "the processes",
            serde_json::to_string(&metrics.processes(env)),
        )
    } else {
        ("the metrics", serde_json::to_string(&metrics.sample(env)))
    };

    match body {
        Ok(json) => Response::json(json),
        Err(error) => Response::text(500, format!("could not serialise {what}: {error}\n")),
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

    // Created before the network is brought up, because the rejoin below reports into it
    // and the page polls it: a wireless network that fails to come back has to be visible
    // in the card rather than only in a log nobody is reading at boot.
    let served_wifi = std::sync::Arc::new(crate::wifi::Job::new());
    crate::wifi::spawn_rejoin(&served_wifi);

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
    identity_for(&addresses, log)?;
    // And reissued when the machine's addresses change, which on DHCP they do. The key is
    // kept, so the fingerprint does not move.
    watch_addresses();

    if let Some(cleartext) = cleartext {
        std::thread::spawn(move || {
            let mut log = |line: &str| println!("plexosd: {line}");
            let _ = http::serve_redirect(&cleartext, &mut log);
        });
    }

    // The credential is read once, here, and what it is decides how the console
    // behaves rather than merely what it logs: an unclaimed device refuses every
    // mutating route outright (ADR-0013).
    let (credential, first_boot_code) =
        claim(std::path::Path::new(crate::auth::CREDENTIAL_FILE), log);

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

    // After the network, so the first frame already carries the address a browser should
    // be pointed at; before `serve_tls`, which never returns.
    spawn_dashboard(first_boot_code);

    let uploading_job = std::sync::Arc::clone(&job);
    let uploading_plex = std::sync::Arc::clone(&plex);
    // One set for the whole daemon. The sampler in here is the reason it cannot be built
    // per request: the previous reading of `/proc` is what every percentage on the activity
    // card is a difference from.
    let services = Services {
        provision: std::sync::Arc::clone(&job),
        plex,
        update,
        install: std::sync::Arc::new(crate::install::Job::new()),
        wifi: served_wifi,
        metrics: std::sync::Arc::new(crate::metrics::Sampler::new()),
    };
    http::serve_tls(
        &listener,
        credential,
        move |request| respond(request, &System, &services),
        move |request, reader| upload_route(request, reader, &uploading_job, &uploading_plex),
        log,
    )
}

/// Starts the screen attached to the machine (ADR-0019).
///
/// On a thread inside this process rather than as a service of its own, and that is the
/// security design rather than an economy: the pairing offer and the sessions it produces
/// live in this process's memory, so nothing has to be written to `/run`, nothing has to be
/// kept in step across a process boundary, and there is no window in which two things could
/// spend one code.
///
/// Its failure is a machine with no monitor, which is most of them. It is logged and the
/// appliance carries on, because it always has.
fn spawn_dashboard(first_boot_code: Option<String>) {
    std::thread::spawn(move || {
        let mut log = |line: &str| println!("plexosd: dashboard: {line}");
        crate::dashboard::run(first_boot_code, &mut log);
    });
}

/// Everything `GET /api/provision` answers: the job, and the machine around it.
///
/// The store is read on every poll rather than cached, which is a directory listing and a
/// `readlink` per second. That is cheap, and the alternative -- state held in the daemon --
/// is the thing that goes stale exactly when it matters, because the version on disk
/// changes underneath it during an install.
fn provision_report(
    job: &std::sync::Arc<crate::provision::Job>,
    plex: &std::sync::Arc<crate::plex::Handle>,
) -> crate::provision::Report {
    let store = crate::appmount::read_store(std::path::Path::new(plexos_types::paths::PLEX_APPS));
    let current = store.current.clone();

    // Newest first, so the version a person would go back to is the one they read first.
    let mut kept: Vec<String> = store
        .installed
        .iter()
        .filter(|v| Some(*v) != current.as_ref())
        .map(|v| v.raw.clone())
        .collect();
    kept.reverse();

    crate::provision::Report {
        progress: job.snapshot(),
        installed: crate::plex::is_provisioned(std::path::Path::new(
            plexos_types::paths::PLEX_MOUNT,
        )),
        running: plex.is_running(),
        plex_log: plex.log(),
        web: crate::provision::PLEX_WEB,
        installed_version: current.as_ref().map(|v| v.raw.clone()),
        kept_versions: kept,
        latest_version: job.latest(),
    }
}

/// Starts an installation that fetches the package from Plex.
///
/// The other half of ADR-0010 is [`upload_route`], which takes a package the machine was
/// given. Both end in the same pipeline with a file on disk; only how it got there
/// differs.
fn start_download(
    job: &std::sync::Arc<crate::provision::Job>,
    plex: &std::sync::Arc<crate::plex::Handle>,
) -> Response {
    if !job.begin() {
        return Response::text(
            409,
            "An installation is already running on this machine. Watch it at \
             GET /api/provision; starting a second would unpack into the same \
             directory as the first.\n",
        );
    }
    // The job that was just claimed, not a fresh one: the whole point of begin() is that
    // the thread reports into the state the page is polling.
    crate::provision::spawn(
        job,
        plex,
        std::path::PathBuf::from(plexos_types::paths::PLEX_APPS),
        std::path::PathBuf::from(plexos_plex::verify::PLEX_KEYRING),
        crate::provision::Source::Download,
    );
    Response::json("{\"started\":true}")
}

/// Installs from a package already on removable media.
///
/// The copy happens here rather than on the worker thread, and that is deliberate: a
/// medium that cannot be read, or a path that is not on it, is a mistake somebody can fix
/// in the next five seconds — so it is answered in the request rather than turned into a
/// job that fails a moment later on a page they may have navigated away from.
fn start_from_media(
    request: &Request,
    env: &impl plexos_gpu::env::Environment,
    job: &std::sync::Arc<crate::provision::Job>,
    plex: &std::sync::Arc<crate::plex::Handle>,
) -> Response {
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null);
    let Some(path) = body.get("path").and_then(serde_json::Value::as_str) else {
        return Response::text(
            400,
            "Choosing a package needs its path. Scan with GET /api/media and send one \
             of the paths it lists, rather than typing one.\n",
        );
    };

    if !job.begin() {
        return Response::text(
            409,
            "An installation is already running on this machine. Watch it at \
             GET /api/provision.\n",
        );
    }

    let apps = std::path::Path::new(plexos_types::paths::PLEX_APPS);
    let destination = apps.join(crate::provision::PACKAGE_FILE);

    match crate::media::fetch(env, path, &destination) {
        Err(error) => {
            let _ = std::fs::remove_file(&destination);
            // The job was claimed a moment ago and nothing else will end it, so it has to
            // be released here or the console reports an installation for ever.
            job.finish(Err(error.to_string()));
            Response::text(400, format!("{error}\n"))
        }
        Ok(bytes) => {
            job.note(&format!(
                "copied {} MiB from removable media; nothing was downloaded",
                bytes / (1024 * 1024)
            ));
            crate::provision::spawn(
                job,
                plex,
                apps.to_path_buf(),
                std::path::PathBuf::from(plexos_plex::verify::PLEX_KEYRING),
                crate::provision::Source::Supplied,
            );
            Response::json("{\"started\":true}")
        }
    }
}

/// The one route that reads the socket itself, for the one thing too big to hold.
///
/// Everything else this console accepts is a few hundred bytes of JSON, which is why
/// `http::MAX_BODY` is 64 KiB and why that number must not move: a hostile
/// `Content-Length` must never be able to name an allocation. Plex's package is 83 MB, so
/// it goes to disk in chunks as it arrives and is never a buffer at all.
///
/// Returns `None` for every request that is not this one, which is what leaves the
/// ordinary path untouched.
///
/// The caller has already applied the token policy — `http::refusal` runs before this, so
/// an unauthenticated client cannot make the appliance receive eighty megabytes and then
/// be told 401.
fn upload_route(
    request: &Request,
    reader: &mut dyn std::io::BufRead,
    job: &std::sync::Arc<crate::provision::Job>,
    plex: &std::sync::Arc<crate::plex::Handle>,
) -> Option<Response> {
    if request.method != "POST" || request.path != crate::provision::UPLOAD_PATH {
        return None;
    }

    // Claimed before a byte is read. Two uploads into one directory would interleave
    // into a file that is neither package, and the second would be verified as though
    // it were whole.
    if !job.begin() {
        return Some(Response::text(
            409,
            "An installation is already running. Watch it at GET /api/provision; \
             starting a second would unpack into the same directory as the first.\n",
        ));
    }

    let apps = std::path::Path::new(plexos_types::paths::PLEX_APPS);
    let package = apps.join(crate::provision::PACKAGE_FILE);

    let outcome = std::fs::create_dir_all(apps)
        .and_then(|()| std::fs::File::create(&package))
        .and_then(|mut file| crate::http::stream_body(reader, request, &mut file));

    match outcome {
        Err(error) => {
            // The partial file is removed rather than left: a truncated .deb that
            // survives is one the next run would verify and report as a bad signature,
            // blaming Plex for a transfer this machine dropped.
            let _ = std::fs::remove_file(&package);
            job.finish(Err(format!(
                "the upload could not be written to {}: {error}. Nothing was installed.",
                package.display()
            )));
            Some(Response::text(
                500,
                "The package could not be written to disk. /var may be full or \
                 read-only; GET /api/status reports whether it is writable.\n",
            ))
        }
        Ok(Err(_)) => {
            let _ = std::fs::remove_file(&package);
            job.finish(Err(
                "the upload declared no length, or one beyond what this console \
                 accepts. Nothing was installed."
                    .to_owned(),
            ));
            Some(Response::text(
                413,
                "That upload declares no Content-Length, or more than this console \
                 accepts. Send the .deb as the raw request body — not a form — and \
                 check it is the package rather than an archive containing it.\n",
            ))
        }
        Ok(Ok(bytes)) => {
            job.note(&format!(
                "received {} MiB from a browser; nothing was downloaded",
                bytes / (1024 * 1024)
            ));
            crate::provision::spawn(
                job,
                plex,
                apps.to_path_buf(),
                std::path::PathBuf::from(plexos_plex::verify::PLEX_KEYRING),
                crate::provision::Source::Supplied,
            );
            Some(Response::json("{\"started\":true}"))
        }
    }
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

/// The disks on this machine, with the one MediaLith runs from marked.
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
/// Everything at `/api/wifi`, read and write.
fn wifi_route(
    request: &Request,
    env: &impl Environment,
    wifi: &std::sync::Arc<crate::wifi::Job>,
) -> Response {
    if request.method == "POST" {
        return wifi_action(request, wifi);
    }
    match serde_json::to_string(&crate::wifi::report(env, wifi)) {
        Ok(body) => Response::json(body),
        Err(error) => Response::text(500, format!("could not describe wireless: {error}\n")),
    }
}

/// Scans, joins or forgets, from `POST /api/wifi`.
///
/// Its own function because `respond` is a router and this is a decision tree; keeping it
/// inline pushed that router past the length at which anybody reads it as one.
///
/// Nothing here does its work inside the request. `http::IO_TIMEOUT` is fifteen seconds
/// and an association is allowed twenty-five, so a request that waited would be cut off in
/// exactly the case worth reporting: a wrong passphrase, which the supplicant retries
/// rather than refuses, so the timeout *is* the error path.
fn wifi_action(request: &Request, wifi: &std::sync::Arc<crate::wifi::Job>) -> Response {
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null);
    match body.get("action").and_then(serde_json::Value::as_str) {
        Some("scan") => {
            if !wifi.begin(crate::wifi::Phase::Scanning, "scanning for networks") {
                return Response::text(
                    409,
                    "The radio is busy. Watch it at GET /api/wifi; a second scan \
                             would interrupt the first.\n",
                );
            }
            crate::wifi::spawn_scan(wifi);
            Response::json("{\"started\":true}")
        }
        Some("join") => {
            let Some(ssid) = body.get("ssid").and_then(serde_json::Value::as_str) else {
                return Response::text(
                    400,
                    "join needs an ssid. Remedy: choose a network from a scan, or \
                             type the name of a hidden one.\n",
                );
            };
            if ssid.is_empty() {
                return Response::text(
                    400,
                    "the network name is empty. Remedy: a hidden network still has \
                             a name, and it has to be typed.\n",
                );
            }
            if !wifi.begin(crate::wifi::Phase::Associating, &format!("joining {ssid}")) {
                return Response::text(409, "The radio is busy. Watch it at GET /api/wifi.\n");
            }
            let passphrase = body
                .get("passphrase")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let hidden = body
                .get("hidden")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            // What the network expects, as the scan reported it. Absent when the name was
            // typed rather than chosen, and then the join keeps the credential that works
            // for both kinds.
            let security = body
                .get("security")
                .and_then(serde_json::Value::as_str)
                .and_then(|kind| {
                    serde_json::from_value(serde_json::Value::String(kind.to_owned())).ok()
                });
            crate::wifi::spawn_join(
                wifi,
                ssid.to_owned(),
                passphrase.to_owned(),
                hidden,
                security,
            );
            Response::json("{\"started\":true}")
        }
        Some("forget") => match crate::wifi::forget() {
            Ok(()) => Response::json("{\"forgotten\":true}"),
            Err(error) => Response::text(500, format!("could not forget it: {error}\n")),
        },
        other => Response::text(
            400,
            format!(
                "{other:?} is not a wireless action. Remedy: one of scan, join, \
                         forget.\n"
            ),
        ),
    }
}

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
             erased. Remedy: none from the console -- MediaLith finds its own disk behind the \
             verified /usr, and not finding it means this is not a booted MediaLith system.\n",
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

    let source = match crate::install::Source::resolve(&source_disk, crate::update::running_slot())
    {
        Ok(source) => source,
        Err(error) => {
            return Response::text(
                500,
                format!(
                    "this system's own partitions could not be found ({error}), so there \
                     is nothing to copy. This is not a MediaLith disk.\n"
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
fn identity_for(addresses: &[String], log: &mut dyn FnMut(&str)) -> io::Result<()> {
    let names = crate::tls::names_for(addresses, &hostname());
    let identity =
        crate::tls::load_or_create(std::path::Path::new(plexos_types::paths::TLS_DIR), &names)?;
    crate::tls::remember(&identity.fingerprint);
    // What it was issued for, so the pairing QR can point at an address this certificate
    // vouches for rather than at whichever one sysfs happened to list first.
    crate::tls::remember_names(&names);

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

    crate::tls::install(crate::tls::server_config(&identity)?);
    Ok(())
}

/// Reissues the certificate when the machine's addresses change.
///
/// The certificate names the addresses the machine has, and until this existed it named the
/// ones it had *at the moment the console started* — for ever. That is wrong on an appliance
/// whose address comes from DHCP, and it was wrong on a real machine within a day: one of
/// them ended up answering at `192.168.2.190` under a certificate for `192.168.2.102`,
/// because the wired adapter it had booted with was unplugged.
///
/// Nothing a browser does breaks — a self-signed certificate warns either way. What breaks
/// is the only thing that makes a self-signed certificate mean anything: comparing the
/// fingerprint at `/api/status` against what the browser was shown. Those were about
/// different addresses.
///
/// **The key is kept**, which is the property that makes this safe to do on a timer.
/// `load_or_create` reissues under the existing key, so the fingerprint does not move and
/// nobody who has already checked it is asked to check it again. A certificate that changed
/// its fingerprint whenever a router handed out a different address would teach exactly one
/// lesson: that the warning means nothing.
///
/// A minute is often enough. A lease changes in seconds and is then stable for hours, and
/// the cost of noticing late is a certificate naming an address the machine no longer has —
/// which is the state this replaces, so a minute of it is not a regression.
fn watch_addresses() {
    /// How often the machine is asked what addresses it has.
    const EVERY: std::time::Duration = std::time::Duration::from_secs(60);

    std::thread::spawn(move || {
        let mut log = |line: &str| println!("plexosd: tls: {line}");
        loop {
            std::thread::sleep(EVERY);

            let addresses: Vec<String> = crate::net::addresses(&System)
                .iter()
                .map(|a| a.ip().to_owned())
                .collect();
            let wanted = crate::tls::names_for(&addresses, &hostname());

            // Compared against what is in force rather than against what was last seen, so
            // a reissue that failed is retried rather than remembered as done.
            if crate::tls::issued_for() == wanted {
                continue;
            }

            match identity_for(&addresses, &mut log) {
                Ok(()) => log(&format!(
                    "the certificate now names {}",
                    if addresses.is_empty() {
                        "no address; this machine has none".to_owned()
                    } else {
                        addresses.join(", ")
                    }
                )),
                Err(error) => log(&format!(
                    "could not reissue the certificate for {}: {error}. The console keeps \
                     serving the one it has, which may name an address this machine no \
                     longer answers at.",
                    addresses.join(", ")
                )),
            }
        }
    });
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
        respond(request, env, &Services::new())
    }

    fn get(path: &str) -> Request {
        Request {
            method: "GET".to_owned(),
            path: path.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A `POST` with a JSON body, for the routes that take one.
    fn post(path: &str, body: &str) -> Request {
        Request {
            method: "POST".to_owned(),
            path: path.to_owned(),
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    /// What `POST /api/pair` answered, as the page would read it.
    fn pair_with(code: &str) -> (u16, String) {
        let response = respond_test(
            &post(crate::http::PAIR_ROUTE, &format!("{{\"code\":\"{code}\"}}")),
            &Fixture::new(),
        );
        (
            response.status,
            String::from_utf8_lossy(&response.body).into_owned(),
        )
    }

    #[test]
    fn an_appliance_nobody_has_touched_pairs_with_nothing() {
        // The state every machine on the LAN sees for all but five minutes of its life,
        // and the reason this route can be unauthenticated: there is nothing to spend.
        let _serialised = crate::pairing::test_lock();
        let (status, body) = pair_with("ANYTHINGATALL");
        assert_eq!(status, 403);
        assert!(body.contains("Remedy:"), "{body}");
        assert!(
            body.contains("press P"),
            "and the remedy is the physical action, which is the whole security model: \
             {body}"
        );
    }

    #[test]
    fn the_code_on_the_screen_pairs_once_and_then_never_again() {
        // The end-to-end shape of ADR-0019 through the route table: a code that only the
        // attached screen could have produced becomes a session, and the second browser to
        // present the same code gets nothing.
        let _serialised = crate::pairing::test_lock();
        let code = crate::pairing::start().expect("/dev/urandom");

        let (status, body) = pair_with(&code);
        assert_eq!(status, 200, "{body}");

        let issued: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(issued["authenticated"], serde_json::json!(true));
        assert_eq!(
            issued["expires_in"],
            serde_json::json!(crate::session::ABSOLUTE_LIFETIME.as_secs())
        );
        assert_eq!(
            issued["idle_timeout"],
            serde_json::json!(crate::session::IDLE_TIMEOUT.as_secs())
        );

        let session = issued["session_token"].as_str().expect("a session token");
        assert_eq!(session.len(), crate::session::TOKEN_BYTES * 2);
        assert_ne!(session, code, "the session is not the pairing code renamed");

        let (again, _) = pair_with(&code);
        assert_eq!(
            again, 403,
            "single use, and the second browser is a stranger"
        );

        assert!(crate::session::revoke(session), "cleanup: it was live");
    }

    #[test]
    fn nothing_in_a_pairing_reply_carries_a_credential_it_should_not() {
        // The list is short and every item on it has a reason to be tempting: the recovery
        // code because the route is about credentials, the Plex token because it is the
        // other secret this daemon holds, and the fingerprints because they are what the
        // server actually compares.
        let _serialised = crate::pairing::test_lock();
        let code = crate::pairing::start().expect("/dev/urandom");
        let (_, body) = pair_with(&code);

        for forbidden in ["device-token", "PlexOnlineToken", "fingerprint", "digest"] {
            assert!(
                !body.contains(forbidden),
                "{forbidden} must not appear: {body}"
            );
        }

        let issued: serde_json::Value = serde_json::from_str(&body).unwrap();
        let session = issued["session_token"].as_str().unwrap();
        assert!(crate::session::revoke(session));
    }

    #[test]
    fn a_pairing_body_that_is_not_what_it_should_be_is_refused_without_a_hint() {
        // And in particular a very long one is refused as wrong rather than as too long:
        // telling a caller which of their inputs had the wrong *shape* is a way to learn
        // the shape.
        let _serialised = crate::pairing::test_lock();
        let _ = crate::pairing::start().expect("/dev/urandom");

        assert_eq!(
            respond_test(&post(crate::http::PAIR_ROUTE, "not json"), &Fixture::new()).status,
            400
        );
        assert_eq!(
            respond_test(
                &post(crate::http::PAIR_ROUTE, "{\"nope\":1}"),
                &Fixture::new()
            )
            .status,
            400
        );

        let (status, body) = pair_with(&"A".repeat(4096));
        assert_eq!(status, 403);
        assert!(!body.contains("long"), "{body}");
        crate::pairing::cancel();
    }

    #[test]
    fn a_pairing_refusal_never_repeats_what_was_presented() {
        // The rule that keeps a wrong code out of a log and out of a browser's history. It
        // holds by construction -- Refusal carries no input -- and this is what stops that
        // becoming untrue later.
        let _serialised = crate::pairing::test_lock();
        let _ = crate::pairing::start().expect("/dev/urandom");
        let guess = "WRONGCODE7T8BHVWPQ2M4X6Z";
        let (status, body) = pair_with(guess);

        assert_eq!(status, 403);
        assert!(!body.contains(guess), "the guess is echoed back: {body}");
        crate::pairing::cancel();
    }

    #[test]
    fn reaching_the_session_route_is_itself_the_answer_to_being_signed_in() {
        // No implementation, and that is the point: the route is a POST, so the gate
        // decided before the handler existed. A second implementation of "are you an
        // administrator" is a second thing to keep in agreement with the gate.
        let response = respond_test(
            &post("/api/session", "{\"action\":\"check\"}"),
            &Fixture::new(),
        );
        assert_eq!(response.status, 200);
        assert!(String::from_utf8_lossy(&response.body).contains("\"authenticated\":true"));
    }

    #[test]
    fn signing_out_ends_the_session_the_request_arrived_with() {
        let _serialised = crate::pairing::test_lock();
        let code = crate::pairing::start().expect("/dev/urandom");
        let (_, body) = pair_with(&code);
        let session = serde_json::from_str::<serde_json::Value>(&body).unwrap()["session_token"]
            .as_str()
            .unwrap()
            .to_owned();

        let mut request = post("/api/session", "{\"action\":\"sign-out\"}");
        request
            .headers
            .push(("Authorization".to_owned(), format!("Bearer {session}")));

        let response = respond_test(&request, &Fixture::new());
        assert_eq!(response.status, 200);
        assert!(String::from_utf8_lossy(&response.body).contains("\"signed_out\":true"));

        // And it is gone from the gate, which is the only place that matters.
        assert!(
            crate::auth::authenticate(
                &session,
                &crate::auth::Credential::Set(crate::auth::fingerprint("4K7QM2XR9T8BHVWP"))
            )
            .is_none()
        );
    }

    #[test]
    fn signing_out_with_a_recovery_code_revokes_nothing_and_is_not_an_error() {
        // There is no server-side state behind the recovery code to remove. The browser
        // drops its own copy either way, which is the outcome the caller wanted.
        let mut request = post("/api/session", "{\"action\":\"sign-out\"}");
        request.headers.push((
            "Authorization".to_owned(),
            "Bearer 4K7Q-M2XR-9T8B-HVWP".to_owned(),
        ));

        let response = respond_test(&request, &Fixture::new());
        assert_eq!(response.status, 200);
        assert!(String::from_utf8_lossy(&response.body).contains("\"signed_out\":false"));
    }

    /// `POST /api/browser-pair/start`, as an unauthenticated desktop would send it.
    fn browser_pair_start_as(agent: &str) -> serde_json::Value {
        let mut request = post(crate::http::BROWSER_PAIR_START, "{}");
        request
            .headers
            .push(("User-Agent".to_owned(), agent.to_owned()));
        let response = respond_test(&request, &Fixture::new());
        assert_eq!(response.status, 200);
        serde_json::from_slice(&response.body).expect("json")
    }

    fn browser_pair_post(path: &str, body: &str) -> (u16, serde_json::Value) {
        let response = respond_test(&post(path, body), &Fixture::new());
        let parsed = serde_json::from_slice(&response.body).unwrap_or_else(
            |_| serde_json::json!({"body": String::from_utf8_lossy(&response.body)}),
        );
        (response.status, parsed)
    }

    #[test]
    fn the_desktops_qr_carries_the_request_and_never_the_secret() {
        // The whole two-value design, asserted where it can actually be got wrong. The id
        // is on a monitor for anybody to photograph; the secret is the thing that makes the
        // photograph worthless, and it must appear nowhere in what is drawn.
        let _serialised = crate::browserpair::test_lock();
        let opened = browser_pair_start_as("Mozilla/5.0 (Windows NT 10.0) Chrome/120");

        let url = opened["url"].as_str().expect("a url");
        let id = opened["request_id"].as_str().expect("an id");
        let secret = opened["desktop_secret"].as_str().expect("a secret");

        assert!(url.contains(&format!("#approve={id}")), "{url}");
        assert!(
            !url.contains(secret),
            "the secret must not be in the QR: {url}"
        );
        assert!(!url.contains('?'), "never a query parameter: {url}");
        assert_ne!(id, secret);

        // And nothing else that authenticates anything is in there either.
        for forbidden in ["session_token", "device-token", "Bearer"] {
            assert!(!url.contains(forbidden), "{forbidden} in {url}");
        }

        // The matrix is the appliance's own encoder, so the page paints rather than
        // encodes -- one implementation of ISO/IEC 18004 in this product, not two.
        let rows = opened["qr"].as_array().expect("a matrix");
        assert!(rows.len() >= 29, "a symbol, {} rows", rows.len());
        for row in rows {
            let row = row.as_str().expect("a row of 1s and 0s");
            assert_eq!(row.len(), rows.len(), "square");
            assert!(row.bytes().all(|b| b == b'0' || b == b'1'), "{row}");
        }
    }

    #[test]
    fn a_waiting_browser_gets_nothing_until_an_administrator_says_yes() {
        // The state an attacker most wants to shortcut, through the routes rather than the
        // store: the desktop has asked, holds its own secret, and is still nobody.
        let _serialised = crate::browserpair::test_lock();
        let opened = browser_pair_start_as("Chrome/120");
        let id = opened["request_id"].as_str().unwrap();
        let secret = opened["desktop_secret"].as_str().unwrap();

        let body = format!(r#"{{"request_id":"{id}","desktop_secret":"{secret}"}}"#);
        let (status, answer) = browser_pair_post(crate::http::BROWSER_PAIR_REDEEM, &body);
        assert_eq!(status, 200, "polling is not an error");
        assert_eq!(answer["status"], "pending");
        assert!(
            answer["session_token"].is_null(),
            "no token before approval"
        );
    }

    #[test]
    fn approval_lets_the_desktop_collect_a_session_of_its_own_exactly_once() {
        let _serialised = crate::browserpair::test_lock();
        let opened = browser_pair_start_as("Chrome/120");
        let id = opened["request_id"].as_str().unwrap().to_owned();
        let secret = opened["desktop_secret"].as_str().unwrap().to_owned();

        let (status, _) = browser_pair_post(
            "/api/browser-pair/approve",
            &format!(r#"{{"request_id":"{id}"}}"#),
        );
        assert_eq!(status, 200);

        let redeem = format!(r#"{{"request_id":"{id}","desktop_secret":"{secret}"}}"#);
        let (status, answer) = browser_pair_post(crate::http::BROWSER_PAIR_REDEEM, &redeem);
        assert_eq!(status, 200);
        assert_eq!(answer["status"], "approved");

        let session = answer["session_token"].as_str().expect("a session");
        assert_eq!(session.len(), crate::session::TOKEN_BYTES * 2);
        // An ordinary session with the ordinary deadlines, not a second kind of credential.
        assert_eq!(
            answer["expires_in"],
            serde_json::json!(crate::session::ABSOLUTE_LIFETIME.as_secs())
        );

        // And it really opens doors, which is the only test of a credential that matters.
        let claimed = crate::auth::Credential::Set(crate::auth::fingerprint("4K7QM2XR9T8BHVWP"));
        assert_eq!(
            crate::auth::authenticate(session, &claimed),
            Some(crate::auth::Principal::AdminSession)
        );

        // Replay: the same approved request, the same correct secret, and nothing.
        let (_, again) = browser_pair_post(crate::http::BROWSER_PAIR_REDEEM, &redeem);
        assert_eq!(again["status"], "refused");
        assert!(again["session_token"].is_null());

        assert!(crate::session::revoke(session), "cleanup: it was live");
    }

    #[test]
    fn the_session_the_desktop_gets_is_not_the_one_the_approver_holds() {
        // The principle this feature is built around: the phone is not a relay. There is no
        // path by which its token could reach the desktop, and this is what says so at the
        // boundary where somebody might one day add one.
        let _serialised = crate::browserpair::test_lock();
        let approver = crate::session::issue().expect("/dev/urandom");

        let opened = browser_pair_start_as("Chrome/120");
        let id = opened["request_id"].as_str().unwrap().to_owned();
        let secret = opened["desktop_secret"].as_str().unwrap().to_owned();

        // The approver's own request carries its session in a header, and the reply says
        // nothing about it.
        let mut approve = post(
            "/api/browser-pair/approve",
            &format!(r#"{{"request_id":"{id}"}}"#),
        );
        approve
            .headers
            .push(("Authorization".to_owned(), format!("Bearer {approver}")));
        let response = respond_test(&approve, &Fixture::new());
        let said = String::from_utf8_lossy(&response.body);
        assert!(
            !said.contains(&approver),
            "the approver's token came back: {said}"
        );

        let (_, answer) = browser_pair_post(
            crate::http::BROWSER_PAIR_REDEEM,
            &format!(r#"{{"request_id":"{id}","desktop_secret":"{secret}"}}"#),
        );
        let desktop = answer["session_token"].as_str().expect("a session");
        assert_ne!(
            desktop, approver,
            "the desktop was handed the phone's session"
        );

        // Two independent sessions: ending one leaves the other.
        assert!(crate::session::revoke(&approver));
        let claimed = crate::auth::Credential::Set(crate::auth::fingerprint("4K7QM2XR9T8BHVWP"));
        assert!(
            crate::auth::authenticate(desktop, &claimed).is_some(),
            "signing the phone out must not sign the desktop out"
        );
        assert!(crate::session::revoke(desktop), "cleanup");
    }

    #[test]
    fn a_refusal_reaches_the_desktop_rather_than_leaving_it_waiting() {
        // Somebody tapped Deny. The desktop should say so within a poll, not sit on
        // "waiting for approval" for the remaining four minutes.
        let _serialised = crate::browserpair::test_lock();
        let opened = browser_pair_start_as("Chrome/120");
        let id = opened["request_id"].as_str().unwrap().to_owned();
        let secret = opened["desktop_secret"].as_str().unwrap().to_owned();

        browser_pair_post(
            "/api/browser-pair/deny",
            &format!(r#"{{"request_id":"{id}"}}"#),
        );

        let (_, answer) = browser_pair_post(
            crate::http::BROWSER_PAIR_REDEEM,
            &format!(r#"{{"request_id":"{id}","desktop_secret":"{secret}"}}"#),
        );
        assert_eq!(answer["status"], "denied");
        assert!(answer["session_token"].is_null());

        // And a refusal cannot be turned into an approval afterwards.
        let (status, _) = browser_pair_post(
            "/api/browser-pair/approve",
            &format!(r#"{{"request_id":"{id}"}}"#),
        );
        assert_eq!(status, 409);
    }

    #[test]
    fn cancelling_needs_the_secret_so_a_passer_by_cannot_stop_somebody_pairing() {
        // The id is on a monitor. If cancelling took only the id, anybody on the network
        // who read one off a screen could stop that pairing, repeatedly.
        let _serialised = crate::browserpair::test_lock();
        let opened = browser_pair_start_as("Chrome/120");
        let id = opened["request_id"].as_str().unwrap().to_owned();
        let secret = opened["desktop_secret"].as_str().unwrap().to_owned();

        let (_, answer) = browser_pair_post(
            crate::http::BROWSER_PAIR_CANCEL,
            &format!(r#"{{"request_id":"{id}","desktop_secret":"read-off-the-screen"}}"#),
        );
        assert_eq!(answer["cancelled"], serde_json::json!(false));

        // Still there for the browser that owns it.
        let (_, answer) = browser_pair_post(
            crate::http::BROWSER_PAIR_CANCEL,
            &format!(r#"{{"request_id":"{id}","desktop_secret":"{secret}"}}"#),
        );
        assert_eq!(answer["cancelled"], serde_json::json!(true));
    }

    #[test]
    fn an_administrator_is_shown_what_is_asking_and_the_number_to_compare() {
        let _serialised = crate::browserpair::test_lock();
        let opened =
            browser_pair_start_as("Mozilla/5.0 (Windows NT 10.0; Win64) Chrome/120 Safari/537");
        let id = opened["request_id"].as_str().unwrap().to_owned();

        let (status, described) = browser_pair_post(
            "/api/browser-pair/inspect",
            &format!(r#"{{"request_id":"{id}"}}"#),
        );
        assert_eq!(status, 200);
        assert_eq!(described["browser"], "Chrome on Windows");
        // The same four digits both screens show, derived from the request rather than
        // reported by either of them.
        assert_eq!(described["verification"], opened["verification"]);

        // And nothing that could be mistaken for a credential.
        let said = described.to_string();
        assert!(!said.contains("secret"), "{said}");
        assert!(!said.contains("session"), "{said}");
    }

    #[test]
    fn inspecting_something_that_is_not_waiting_says_so_without_a_hint() {
        let _serialised = crate::browserpair::test_lock();
        let (status, _) = browser_pair_post(
            "/api/browser-pair/inspect",
            r#"{"request_id":"nothing-by-that-name"}"#,
        );
        assert_eq!(status, 404);
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
        // "https://" is not in this list: the page has exactly one absolute URL and the
        // test below asks the sharper question about it -- whether it points at this
        // machine. A blanket ban here would only be a second, blunter copy of that.
        for marker in ["<script src", "<link", "<img", "@import"] {
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
            let response = respond(&request, &Fixture::new(), &Services::new());
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
    fn the_page_offers_the_offline_install_and_aims_it_at_the_streaming_route() {
        // ADR-0010 requires an offline path -- "a media server in a cupboard may well
        // have no outbound internet at setup time, and this must not be a dead end" --
        // and until now the page offered only the download. A route with no control is a
        // route nobody uses, which is the same shape as the auth gate that nothing ever
        // called.
        assert!(
            PAGE.contains(&format!("\"{}\"", crate::provision::UPLOAD_PATH)),
            "the page must post to the streaming route, spelled the same way"
        );
        assert!(
            PAGE.contains("id=\"plex-package\""),
            "and offer a file picker to choose the package with"
        );
        assert!(
            PAGE.contains("accept=\".deb\""),
            "restricted to the package Plex publishes, so the wrong file is harder to send"
        );
        assert!(
            PAGE.contains("the signature is checked exactly as it is for a download"),
            "and must say that an offline install is not the weaker one, because there is \
             no other reason for somebody to believe it"
        );
    }

    #[test]
    fn the_upload_is_sent_as_a_raw_body_rather_than_a_form() {
        // The server has no multipart parser and must never grow one: it would stand
        // between the network and a signature check, in a hand-written HTTP server. A
        // page that sent FormData would be read as raw bytes anyway, so the package
        // would arrive wrapped in boundaries and fail its signature -- an error blaming
        // Plex for what the page did.
        assert!(
            !PAGE.contains("FormData"),
            "a multipart body would need a parser this server does not have"
        );
        assert!(
            PAGE.contains("application/octet-stream"),
            "the file goes as the request body itself"
        );
    }

    #[test]
    fn the_page_can_look_for_a_package_on_a_stick() {
        // The last route in this project to be built and left uncalled was the auth gate,
        // and it made every mutating request answer 503 for months. A media scanner with
        // no button on the page is the same defect wearing different clothes.
        assert!(
            PAGE.contains("\"/api/media\""),
            "the page must be able to scan"
        );
        assert!(
            PAGE.contains("\"/api/provision/media\""),
            "and to install what the scan found"
        );
        assert!(
            PAGE.contains("id=\"plex-media-scan\""),
            "with a control somebody can press"
        );
    }

    #[test]
    fn the_page_never_asks_somebody_to_type_a_path() {
        // The route vets what it is given, and the vetting is what stops it reading
        // /var/lib/plexos/device-token. A free-text path field would make that check the
        // only thing standing between a typo and a refusal somebody would read as a bug,
        // and would invite exactly the input the check exists to reject.
        assert!(
            !PAGE.contains("id=\"media-path\""),
            "packages are chosen from the scan, never typed"
        );
        assert!(
            PAGE.contains("data-path="),
            "the choice carries the path the scan reported"
        );
    }

    #[test]
    fn the_pages_token_field_folds_exactly_as_the_server_does() {
        // One rule, now written twice in two languages, and the failure is silent in the
        // worst way: the field would show one token while the server computed another,
        // and the only symptom is 403 on a token that was typed correctly off the screen.
        // Nobody debugging that would suspect the input field.
        //
        // So the page's own functions are run under a real engine and compared against
        // auth::normalise and auth::grouped, which are the definition.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has exactly one script block");

        let function = |name: &str| {
            let start = script
                .find(&format!("function {name}("))
                .unwrap_or_else(|| panic!("{name} is a named function so it can be tested"));
            let end = script[start..]
                .find("\n}\n")
                .map(|at| start + at + "\n}".len())
                .expect("and ends at a brace in the first column");
            script[start..end].to_owned()
        };
        let constant = |name: &str| {
            script
                .lines()
                .find(|line| line.starts_with(&format!("const {name} = ")))
                .unwrap_or_else(|| panic!("{name} is declared on one line"))
                .to_owned()
        };

        // Everything a person actually does: the printed form, the form with no dashes,
        // lower case, spaces instead of dashes, and the three characters the alphabet
        // leaves out precisely because they are misread.
        let cases = [
            "4K7Q-M2XR-9T8B-HVWP",
            "4k7q-m2xr-9t8b-hvwp",
            "4K7QM2XR9T8BHVWP",
            " smz7 7rs1 wn9h zvvz ",
            "oOiIlL0011",
            "4k7q_m2xr!!9t8b",
            "",
        ];

        let Some(engine) = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            println!(
                "skip: the token field's folding was not compared against auth::normalise \
                 -- no node, deno or qjs on this host. Install one, or a field that \
                 disagrees with the server will only be found by somebody being refused a \
                 token they typed correctly."
            );
            return;
        };

        let program = format!(
            "{}\n{}\n{}\n{}\n{}\nconst cases = {};\nconsole.log(cases.map(c => \
             normaliseToken(c) + \"|\" + groupToken(normaliseToken(c))).join(\"\\n\"));",
            constant("TOKEN_ALPHABET"),
            constant("TOKEN_CHARS"),
            constant("TOKEN_GROUP"),
            function("normaliseToken"),
            function("groupToken"),
            serde_json::to_string(&cases).expect("a list of strings"),
        );

        let output = std::process::Command::new(engine)
            .args(["-e", &program])
            .output()
            .expect("the engine runs");
        assert!(
            output.status.success(),
            "the page's token functions did not run: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let got = String::from_utf8_lossy(&output.stdout);
        for (case, line) in cases.iter().zip(got.lines()) {
            let canonical = crate::auth::normalise(case);
            let expected = format!("{canonical}|{}", crate::auth::grouped(&canonical));
            assert_eq!(
                line, expected,
                "the page and auth:: disagree about {case:?}; the page is what changes"
            );
        }
    }

    #[test]
    fn the_pages_token_field_stops_at_a_whole_token() {
        // The one place the page deliberately does *more* than auth::normalise, which
        // does not cap: sixteen characters is a whole token, and a field that kept
        // accepting them would let somebody type past the end and never see why the
        // result was refused. Recorded here so the divergence above is a decision rather
        // than a gap in the comparison.
        assert!(
            PAGE.contains("if (out.length === TOKEN_CHARS) break;"),
            "the field must stop at a whole token"
        );
        assert!(
            PAGE.contains("const TOKEN_CHARS = 16;"),
            "and sixteen is what auth::TOKEN_CHARS computes to"
        );
        assert_eq!(
            crate::auth::TOKEN_CHARS,
            16,
            "if this changed, the page's copy has to change with it"
        );
    }

    #[test]
    fn nothing_somebody_types_into_lives_inside_the_polled_element() {
        // #main is replaced wholesale every few seconds. A control inside it that holds
        // typed text loses focus mid-word, closes its own datalist, and discards anything
        // not yet saved -- which is how the settings form behaved, reported as "the
        // cursor escapes the field" and "the timezone list has two strange entries",
        // two faces of one defect.
        //
        // The terminal was moved out of #main for this reason and the reasoning was
        // written down at the time. It was not applied to the form, so this asserts the
        // rule rather than the instance: the settings card is built outside the poll.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");

        let start = script
            .find("document.getElementById(\"main\").innerHTML =")
            .expect("the poll replaces #main");
        let end = script[start..]
            .find("`;")
            .map(|at| start + at)
            .expect("and the template it assigns ends");

        assert!(
            !script[start..end].contains("hostedSettingsCard"),
            "the settings form is rendered inside the element the poll replaces, so it \
             will lose focus and typed text on a timer"
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
    fn every_element_the_script_reaches_for_exists_in_the_markup() {
        // Written after shipping a section whose markup was never added. The script called
        // getElementById for it, got null, threw, and the throw was swallowed by the poll's
        // own error handling -- so the endpoint worked, the page rendered, and the feature
        // was simply absent. The tests passed because they asserted that strings appear in
        // the page, and those strings were in the *script*.
        //
        // This is the general form: a page whose script addresses an element that is not
        // there is a page with a silent hole in it, and no assertion about text can see it.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");

        let mut missing = Vec::new();
        for (index, _) in script.match_indices("getElementById(\"") {
            let rest = &script[index + "getElementById(\"".len()..];
            let Some((id, _)) = rest.split_once('"') else {
                continue;
            };
            // Three ways an element legitimately comes to exist: written in the markup,
            // written by a render function into innerHTML, or created and given an id in
            // code. The last was not in the first version of this test and produced two
            // false alarms immediately -- which is the right way round for a check whose
            // job is to be believed.
            let in_markup = PAGE.contains(&format!("id=\"{id}\""));
            let assigned = script.contains(&format!(".id = \"{id}\""))
                || script.contains(&format!(".id=\"{id}\""));
            if !in_markup && !assigned {
                missing.push(id);
            }
        }
        missing.sort_unstable();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "the script addresses elements that nothing creates: {missing:?}"
        );
    }

    #[test]
    fn every_class_the_script_draws_with_is_a_class_the_stylesheet_defines() {
        // The third member of the family above, and it was written because the first two
        // could not see the defect that produced it. Renaming the activity card's helper
        // functions with a word-boundary search-and-replace also renamed the class names
        // sitting in their template literals -- `class="meter"` became `class="metricMeter"`
        // and so on -- and the result was a card that rendered every element, addressed
        // every id, parsed cleanly, and drew as unstyled text. `getElementById` was
        // satisfied, `node --check` was satisfied, and nothing here was looking at the one
        // channel that had broken.
        //
        // A class in the script with no rule for it is not always an error: some exist to be
        // found rather than to be painted. So the rule is "styled, or selected on" rather
        // than "styled", which is a property of the page instead of a list of exceptions
        // that goes stale -- and the first run proved the difference by flagging
        // `media-pick` and `share-drop`, both perfectly legitimate `querySelectorAll` hooks.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");
        let style = PAGE
            .split_once("<style>")
            .and_then(|(_, rest)| rest.split_once("</style>"))
            .map(|(body, _)| body)
            .expect("the page has one style block");

        // `hidden` is the HTML attribute rather than a rule of this sheet's own.
        let behavioural = ["hidden"];

        let mut unstyled = Vec::new();
        for (index, _) in script.match_indices("class=\"") {
            let rest = &script[index + "class=\"".len()..];
            let Some((value, _)) = rest.split_once('"') else {
                continue;
            };
            // A class list is interpolated as often as it is literal, so only the part
            // before the first `${` is a name this can ask about. `class="step ${state}"`
            // yields `step`; the interpolated half is a value, not a name.
            let literal = value.split("${").next().unwrap_or_default();
            for name in literal.split_whitespace() {
                if behavioural.contains(&name) {
                    continue;
                }
                let styled = style.contains(&format!(".{name}"));
                // A selector hook: `querySelectorAll(".media-pick")` and friends, where the
                // class is how the script finds the element again rather than how it looks.
                let selected_on = script.contains(&format!(".{name}\""));
                if !styled && !selected_on {
                    unstyled.push(name);
                }
            }
        }
        unstyled.sort_unstable();
        unstyled.dedup();
        assert!(
            unstyled.is_empty(),
            "the script draws elements with classes the stylesheet never defines, so they \
             render as unstyled text on a page where everything else looks right: \
             {unstyled:?}"
        );
    }

    #[test]
    fn a_running_plex_is_still_offered_a_way_to_install_a_newer_one() {
        // The defect this guards: the `report.running` branch of renderPlexInto rendered a
        // heading, a sentence and a link, then returned. Every control disappeared the
        // moment Plex was installed, so the console could install Plex exactly once and
        // never move it forward again -- while the whole machinery for doing so
        // (`plexos_plex::store`, `plex::swap`) sat complete, tested and uncalled.
        //
        // Asserting on the source is weaker than driving a browser, and it is what catches
        // a `return` put back above the version controls.
        //
        // The two branches became one when the Plex view was built: running and
        // installed-but-not-running differ by a word, a colour and a sentence, and writing
        // that twice is how the running branch came to be missing every control in the first
        // place. So this reads the merged branch, and the property it guards is unchanged.
        let branch = PAGE
            .split("if (report.running || report.installed)")
            .nth(1)
            .expect("the branch that draws an installed Plex");
        let branch = &branch[..branch.find("\n  const progress =").unwrap_or(branch.len())];

        assert!(
            branch.contains("plexVersionsMarkup"),
            "an installed Plex must still offer the version controls: {branch}"
        );
        assert!(
            branch.contains("progressMarkup"),
            "an upgrade that fails while Plex runs has to report it somewhere"
        );
        assert!(
            branch.contains("wirePlexVersionControls"),
            "and the controls it renders have to be wired to something"
        );
    }

    #[test]
    fn the_version_controls_exist_for_every_button_they_wire() {
        // wirePlexVersionControls checks each element before addEventListener, so a missing
        // one is silent. That makes the markup the thing worth asserting on.
        for id in ["plex-check", "plex-reinstall", "plex-update"] {
            assert!(
                PAGE.contains(&format!("id=\"{id}\"")),
                "{id} is wired by the script and created by nothing"
            );
        }
    }

    #[test]
    fn starting_an_install_disables_whichever_button_started_it() {
        // Three buttons now start the same install and they never appear together, so
        // naming one would throw on the two states that do not have it -- inside a poll
        // that swallows the exception, leaving a section that quietly stops updating.
        let body = PAGE
            .split("async function startPlexInstall()")
            .nth(1)
            .expect("startPlexInstall");
        assert!(
            body.contains("plex-install") && body.contains("plex-update"),
            "the install must disable every button that can start it"
        );
    }

    #[test]
    fn no_id_is_given_to_two_elements() {
        // The companion to the test above, and the hole it left. That one asks whether
        // every id the script reaches for exists; this asks whether it names exactly one
        // thing. Both were true of `install` -- the disk installer's section and the Plex
        // card's button were both called that -- and `getElementById` answers with whichever
        // the markup puts first, which was the button. So `renderInstall` wrote the whole
        // "Install to a disk" card *into the Install Plex button*: the button vanished, the
        // real section sat on "Loading...", and there was no way left on the page to install
        // Plex. Found on the appliance, by somebody looking at the page and asking what to
        // press.
        //
        // Every id here is preceded by a space, so the search needs no parser.
        let mut ids: Vec<&str> = PAGE
            .match_indices(" id=\"")
            .filter_map(|(index, _)| {
                PAGE[index + " id=\"".len()..]
                    .split_once('"')
                    .map(|(id, _)| id)
            })
            .collect();
        ids.sort_unstable();
        let duplicated: Vec<&str> = ids
            .windows(2)
            .filter(|pair| pair[0] == pair[1])
            .map(|pair| pair[0])
            .collect();
        assert!(
            duplicated.is_empty(),
            "these ids name more than one element, so getElementById returns whichever \
             comes first in the markup and every render keyed on them lands on the wrong \
             one: {duplicated:?}"
        );
    }

    /// The page's one script block, which most of these tests want.
    fn page_script() -> &'static str {
        PAGE.split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has exactly one script block")
    }

    /// The page's one style block.
    fn page_style() -> &'static str {
        PAGE.split_once("<style>")
            .and_then(|(_, rest)| rest.split_once("</style>"))
            .map(|(body, _)| body)
            .expect("the page has exactly one style block")
    }

    #[test]
    fn every_view_the_sidebar_offers_is_a_section_that_exists() {
        // The console is seven views behind a sidebar, and the two halves are written in two
        // places: `VIEWS` in the script, `data-view` on the buttons, and `data-view` on the
        // sections. A name in one and not the others is a menu item that does nothing, or a
        // section nothing can reach -- and neither throws, because the switch is a loop over
        // elements that simply does not find one.
        let script = page_script();
        let declared = script
            .split_once("const VIEWS = [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(list, _)| list)
            .expect("the views are declared in one place");
        let views: Vec<&str> = declared
            .split(',')
            .map(|name| name.trim().trim_matches('"'))
            .filter(|name| !name.is_empty())
            .collect();
        assert!(views.len() >= 7, "seven views were designed: {views:?}");

        for view in &views {
            assert!(
                PAGE.contains(&format!("<section class=\"view\" data-view=\"{view}\"")),
                "{view} is offered by the sidebar and there is no section for it"
            );
            assert!(
                PAGE.contains(&format!("class=\"navitem\" data-view=\"{view}\"")),
                "{view} is a section with nothing in the sidebar that reaches it"
            );
        }

        // And the reverse: a button naming a view the script does not know about switches to
        // nothing at all, silently.
        for (index, _) in PAGE.match_indices("class=\"navitem\" data-view=\"") {
            let rest = &PAGE[index + "class=\"navitem\" data-view=\"".len()..];
            let (name, _) = rest.split_once('"').expect("a closed attribute");
            assert!(
                views.contains(&name),
                "the sidebar offers {name:?}, which is not one of {views:?}"
            );
        }
    }

    #[test]
    fn switching_views_never_leaves_the_page() {
        // The console is a page people leave open for hours on a second screen, and every
        // poll, the terminal's scrollback and the typed token live in it. A navigation item
        // that was an anchor with an href would throw all of that away on every click -- and
        // would look to a keyboard and a screen reader like something that loads a document,
        // which it must not, because nothing here does.
        let script = page_script();
        assert!(
            !PAGE.contains("<a class=\"navitem\""),
            "a view is switched, not navigated to"
        );
        assert!(
            PAGE.contains("<button class=\"navitem\""),
            "and the control is a button, which is what it behaves like"
        );
        // The address bar still moves, so a link to a view can be sent and the back button
        // steps between them.
        assert!(
            script.contains("history.pushState"),
            "the URL has to follow the view, or #network is not a link anybody can send"
        );
        assert!(
            script.contains("popstate"),
            "and the view has to follow the URL, or the back button does nothing"
        );
    }

    #[test]
    fn the_token_is_behind_a_lock_and_nothing_about_it_moved() {
        // The device token card was the first and largest thing on this page. It is a
        // popover behind a lock in the header now, and this is the test that the *redesign*
        // did not quietly become a change to ADR-0013: the token is still typed into the
        // same field, still kept in sessionStorage and nowhere else, and still sent as an
        // Authorization header.
        assert!(
            PAGE.contains("id=\"token-card\" hidden"),
            "the card starts hidden, so a dashboard does not open with a credential box on it"
        );
        assert!(
            PAGE.contains("id=\"admin-lock\""),
            "and there is a control in the header that opens it"
        );

        let script = page_script();
        assert!(
            script.contains("sessionStorage.getItem(TOKEN_KEY)")
                && script.contains("sessionStorage.setItem(TOKEN_KEY"),
            "the token lives in sessionStorage, which is cleared when the tab closes"
        );
        assert!(
            !script.contains("localStorage.setItem(TOKEN_KEY")
                && !script.contains("localStorage.getItem(TOKEN_KEY"),
            "and never in localStorage, which outlives the tab"
        );
        // This assertion is the reverse of what it was, and the reversal is the point of
        // ADR-0019 rather than a relaxation. Locking used to be a thing this browser did
        // and nothing the appliance was told, which was exactly right while the only
        // credential was the device token: there is no server-side state behind a device
        // token, so there was nothing to end and a request would have been ceremony.
        //
        // A session is not like that. It is state on the appliance, and a "Sign out" that
        // dropped the browser's copy and left the session live would leave a working
        // credential behind -- the one thing somebody pressing that button is trying to
        // prevent. So sign-out now reaches the machine, and what must stay true is that it
        // reaches it *first* and then forgets locally whatever the answer was.
        let lock = script
            .split_once("async function lockAdmin()")
            .map(|(_, rest)| rest)
            .expect("signing out is a named function");
        let lock = &lock[..lock.find("\n}\n").unwrap_or(lock.len())];
        assert!(
            lock.contains("sessionStorage.removeItem(TOKEN_KEY)")
                && lock.contains("forgetSession()"),
            "signing out forgets both credentials in this tab: {lock}"
        );
        assert!(
            lock.contains("/api/session") && lock.contains("sign-out"),
            "and tells the appliance, so the session it holds stops working: {lock}"
        );
        let (asked, forgot) = (
            lock.find("fetch(").expect("it asks"),
            lock.find("forgetSession()").expect("it forgets"),
        );
        assert!(
            asked < forgot,
            "the request goes first: a sign-out that gave up because the network was down \
             would leave the credential in a tab somebody believes they closed: {lock}"
        );
    }

    #[test]
    fn every_request_that_carries_a_credential_asks_one_function_for_it() {
        // ADR-0019's integration on the browser's side, and the one property that keeps it
        // from becoming fifteen decisions: there is one function that answers "what does
        // this tab send", it prefers the session, and nothing reaches for storage itself.
        //
        // Checked by reading every `Bearer` on the page rather than by counting call
        // sites, because a request added tomorrow is the one that would get this wrong.
        let script = page_script();
        let marker = "\"Bearer \" + ";

        let mut carried = Vec::new();
        for (index, _) in script.match_indices(marker) {
            let rest = &script[index + marker.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            carried.push(name);
        }

        assert!(
            carried.len() >= 15,
            "the page sends a credential on many more routes than this: {carried:?}"
        );
        for name in &carried {
            assert!(
                matches!(name.as_str(), "value" | "held" | "typed"),
                "a request built its own credential from {name:?} instead of asking \
                 credential() for one"
            );
        }

        // `typed` is the one exception and it is a narrow one: checking a recovery code as
        // somebody enters it is asking about a credential that is *not yet* in force. Using
        // credential() there would validate the session this browser already holds and
        // report "Unlocked" about the wrong thing entirely -- so the exception exists
        // because the rule, applied literally, would be wrong.
        let checker = script
            .split_once("async function checkTypedToken()")
            .map(|(_, rest)| rest)
            .expect("checking a typed code is a named function");
        let checker = &checker[..checker.find("\n}\n").unwrap_or(checker.len())];
        assert_eq!(
            script.matches("\"Bearer \" + typed").count(),
            1,
            "only one request may present a code that is not the credential in force"
        );
        assert!(
            checker.contains("\"Bearer \" + typed"),
            "and it is the one that checks what was typed"
        );

        assert!(
            !script.contains("const value = token();") && !script.contains("const held = token();"),
            "and the two names those requests use are assigned from credential(), so a \
             paired browser never sends the recovery code in front of its own session"
        );
        assert!(
            script.contains("function credential() { return session() || token(); }"),
            "the session wins, because it is the one the person just established and the \
             one Sign out can end"
        );
    }

    #[test]
    fn running_the_pages_own_code_shows_the_fragment_gone_and_the_code_recovered() {
        // An assertion about the page's text cannot see this, and the page's text has been
        // right while its behaviour was wrong before -- the terminal cleaner destroyed
        // sixty-one per cent of every session's output under tests that all passed. So
        // this runs takePairingCode itself, under a real engine, against the four URLs it
        // will actually meet.
        //
        // The case that matters most is the last: a hash that is not a pairing code must
        // come through untouched, because that hash is how the console chooses which view
        // to show. A function that cleared it unconditionally would send every link
        // somebody sends -- `#network`, `#terminal` -- to the Overview instead.
        let script = page_script();
        let start = script
            .find("function takePairingCode(")
            .expect("taking the code out of the URL is a named function");
        let end = script[start..]
            .find("\n}\n")
            .map(|at| start + at + "\n}".len())
            .expect("and ends at a brace in the first column");
        let function = &script[start..end];

        // The prefix comes from the page too, rather than being written again here. A test
        // that declared its own would agree with itself while the page looked for
        // something else -- which is the fixture-you-imagined trap, and this is the one
        // string that has to match what the appliance puts in the QR code.
        let prefix = script
            .lines()
            .find(|line| line.starts_with("const PAIR_PREFIX = "))
            .expect("the fragment prefix is declared on one line");

        let Some(engine) = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            println!(
                "skip: the pairing bootstrap was not run -- no node, deno or qjs on this \
                 host. Install one, or a fragment left in the address bar will only be \
                 found by scanning a QR code with a phone."
            );
            return;
        };

        // A window just real enough. `history.replaceState` records what it was given,
        // which is the half of the behaviour that has no return value to inspect.
        let harness = format!(
            r##"
{prefix}
{function}

const cases = [
  ["#pair=ABC123", "", "ABC123"],
  ["#pair=", "", ""],
  ["", "", ""],
  ["#network", "#network", ""],
];

for (const [hash, expectedHash, expectedCode] of cases) {{
  let replaced = null;
  globalThis.window = {{ location: {{ hash, pathname: "/", search: "" }} }};
  globalThis.history = {{ replaceState: (_s, _t, url) => {{ replaced = url; }} }};

  const got = takePairingCode();
  if (got !== expectedCode) {{
    console.log("FAIL code " + JSON.stringify(hash) + ": " + JSON.stringify(got));
    process.exit(1);
  }}
  // A hash that is not a pairing code must be left where it is, hence "no replaceState".
  const left = replaced === null ? hash : "";
  if (left !== expectedHash) {{
    console.log("FAIL hash " + JSON.stringify(hash) + ": left " + JSON.stringify(left)
                + ", replaced with " + JSON.stringify(replaced));
    process.exit(1);
  }}
}}
console.log("OK");
"##
        );

        let path = std::env::temp_dir().join("medialith-take-pairing-code-test.js");
        std::fs::write(&path, harness).expect("write the harness");
        let output = std::process::Command::new(engine)
            .arg(&path)
            .output()
            .expect("run the harness");
        let _ = std::fs::remove_file(&path);

        assert!(
            String::from_utf8_lossy(&output.stdout).contains("OK"),
            "the page's own pairing bootstrap misbehaved:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn an_approval_request_leaves_the_url_the_same_way_a_pairing_code_does() {
        // Run rather than asserted about, because this page has shipped correct text over
        // wrong behaviour before. The case that matters most is the last one: a hash that is
        // not an approval request must come through untouched, or every `#network` link
        // somebody sends opens on the Overview instead.
        let script = page_script();
        let start = script
            .find("function takeApprovalRequest(")
            .expect("taking the request out of the URL is a named function");
        let end = script[start..]
            .find("\n}\n")
            .map(|at| start + at + "\n}".len())
            .expect("and ends at a brace in the first column");
        let function = &script[start..end];
        let prefix = script
            .lines()
            .find(|line| line.starts_with("const APPROVE_PREFIX = "))
            .expect("the fragment prefix is declared on one line");

        let Some(engine) = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            println!(
                "skip: the approval bootstrap was not run -- no node, deno or qjs on this \
                 host. Install one, or a request left in the address bar will only be found \
                 by scanning a code with a phone."
            );
            return;
        };

        let harness = format!(
            r##"
{prefix}
{function}

const cases = [
  ["#approve=abc123", "", "abc123"],
  ["#approve=", "", ""],
  ["", "", ""],
  ["#network", "#network", ""],
  ["#pair=SOMECODE", "#pair=SOMECODE", ""],
];

for (const [hash, expectedHash, expectedId] of cases) {{
  let replaced = null;
  globalThis.window = {{ location: {{ hash, pathname: "/", search: "" }} }};
  globalThis.history = {{ replaceState: (_s, _t, url) => {{ replaced = url; }} }};

  const got = takeApprovalRequest();
  if (got !== expectedId) {{
    console.log("FAIL id " + JSON.stringify(hash) + ": " + JSON.stringify(got));
    process.exit(1);
  }}
  const left = replaced === null ? hash : "";
  if (left !== expectedHash) {{
    console.log("FAIL hash " + JSON.stringify(hash) + ": left " + JSON.stringify(left));
    process.exit(1);
  }}
}}
console.log("OK");
"##
        );

        let path = std::env::temp_dir().join("medialith-take-approval-request-test.js");
        std::fs::write(&path, harness).expect("write the harness");
        let output = std::process::Command::new(engine)
            .arg(&path)
            .output()
            .expect("run the harness");
        let _ = std::fs::remove_file(&path);

        assert!(
            String::from_utf8_lossy(&output.stdout).contains("OK"),
            "the page's own approval bootstrap misbehaved:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn the_desktop_secret_travels_in_a_body_and_is_never_drawn() {
        // The half of the two-value design that lives in the browser. The id is in a QR on
        // a monitor; the secret is what makes photographing that monitor useless, so it
        // must never reach a URL, a QR or the screen.
        let script = page_script();

        for (function, marker) in [
            ("async function bpPoll()", "/api/browser-pair/redeem"),
            ("async function bpCancel()", "/api/browser-pair/cancel"),
        ] {
            let body = script
                .split_once(function)
                .map_or_else(|| panic!("{function} exists"), |(_, rest)| rest);
            let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
            assert!(body.contains(marker), "{function} posts to {marker}");
            assert!(
                body.contains("body: JSON.stringify(held)"),
                "{function} sends the request and its secret in the body: {body}"
            );
        }

        // The QR the desktop paints comes from the appliance, and what it paints is a
        // matrix of ones and zeros -- so there is no encoder in this page that could be
        // handed a secret by mistake.
        assert!(
            script.contains("function qrSvg(rows)"),
            "the page paints a matrix rather than encoding one"
        );
        assert!(
            !script.contains("desktop_secret") || script.contains("opened.desktop_secret"),
            "the secret is stored, never drawn"
        );
        let show = script
            .split_once("function bpShow(opened)")
            .map(|(_, rest)| rest)
            .expect("bpShow exists");
        let show = &show[..show.find("\n}\n").unwrap_or(show.len())];
        assert!(
            !show.contains("secret"),
            "nothing about the secret is drawn on the desktop: {show}"
        );
    }

    #[test]
    fn approving_sends_a_sentence_about_a_request_and_never_a_session() {
        // The principle, on the browser's side: the phone tells the appliance which request
        // it approves. It does not hand anything over, and there is no code here that
        // could -- the only thing it sends of its own is the Authorization header every
        // other request on this page carries.
        let script = page_script();
        let decide = script
            .split_once("async function decideApproval(approve)")
            .map(|(_, rest)| rest)
            .expect("deciding is a named function");
        let decide = &decide[..decide.find("\n}\n").unwrap_or(decide.len())];

        assert!(decide.contains("body: JSON.stringify({ request_id: id })"));
        assert!(
            !decide.contains("session_token") && !decide.contains("rememberSession"),
            "the approver must not move a session anywhere: {decide}"
        );
    }

    #[test]
    fn the_pairing_code_leaves_the_url_before_anything_can_read_it() {
        // The fragment is not sent to the server, which is why the QR uses one. What the
        // fragment *is* exposed to is the address bar, the history, a screenshot and --
        // on this page specifically -- the view router, which reads the hash to decide
        // which section to show. All four are closed by taking it out immediately.
        let script = page_script();

        let take = script
            .split_once("function takePairingCode()")
            .map(|(_, rest)| rest)
            .expect("taking the code out of the URL is a named function");
        let take = &take[..take.find("\n}\n").unwrap_or(take.len())];
        assert!(
            take.contains("history.replaceState"),
            "removed with replaceState: assigning location.hash adds a history entry, so \
             Back would put the code straight back: {take}"
        );
        assert!(
            !take.contains("await"),
            "and removed synchronously, before anything can await and let the router run: \
             {take}"
        );

        // Before the router reads the URL. This ordering is the whole reason the call sits
        // where it does, and nothing else in the file would fail if it moved.
        let bootstrap = script
            .find("bootstrapPairing();")
            .expect("the bootstrap runs at start-up");
        let router = script
            .find("showView(viewInUrl());")
            .expect("the router reads the URL at start-up");
        assert!(
            bootstrap < router,
            "the pairing code must be out of the hash before showView looks a view up by it"
        );
    }

    #[test]
    fn a_pairing_code_travels_in_a_body_and_never_in_a_url() {
        // The rule the whole flow is arranged around. A query parameter would put the code
        // in the request line -- and in a browser's history, and in anything that ever logs
        // one -- which is what the fragment exists to avoid.
        let script = page_script();

        let bootstrap = script
            .split_once("async function bootstrapPairing()")
            .map(|(_, rest)| rest)
            .expect("the bootstrap is a named function");
        assert!(
            bootstrap.contains("body: JSON.stringify({ code })"),
            "the code goes in the body of the POST"
        );

        for forbidden in ["?pair=", "?token=", "?session=", "?code=", "?device-token="] {
            assert!(
                !PAGE.contains(forbidden),
                "no credential may appear in a query string, and the page has {forbidden}"
            );
        }
    }

    #[test]
    fn an_administrator_session_is_kept_where_it_dies_with_the_tab() {
        // sessionStorage for the same reason the device token is there, and one more: this
        // credential is meant to end. localStorage would quietly undo the first of the five
        // ways it does.
        let script = page_script();
        assert!(
            script.contains("sessionStorage.getItem(SESSION_KEY)")
                && script.contains("sessionStorage.setItem(SESSION_KEY"),
            "the session lives in sessionStorage"
        );
        assert!(
            !script.contains("localStorage.getItem(SESSION_KEY)")
                && !script.contains("localStorage.setItem(SESSION_KEY"),
            "and never in localStorage, which outlives the tab"
        );
        assert!(
            !script.contains("document.cookie"),
            "and never in a cookie, which would be attached to requests automatically -- \
             the thing ADR-0013 chose a bearer token to avoid"
        );
    }

    #[test]
    fn nothing_that_redraws_can_overwrite_a_session_that_was_just_stored() {
        // This console has already shipped a redraw that closed what somebody had opened
        // and a rename that broke every meter. A poll that wrote to the session key would
        // be the same shape of fault with a credential in it -- the browser would appear
        // to sign itself out at a rate of once every three seconds.
        //
        // The property that prevents it is that exactly one place writes the key, and it is
        // the one that has just been handed a session by the appliance.
        let script = page_script();

        // One definition and exactly two callers. Both are functions the appliance has just
        // handed a session to -- spending a pairing code from the machine's own screen, and
        // collecting one that another browser approved. Anything else writing this key
        // would be the redraw fault with a credential in it.
        assert_eq!(
            script.matches("rememberSession(").count(),
            3,
            "one definition and two callers"
        );
        for (owner, marker) in [
            ("the pairing bootstrap", "async function bootstrapPairing()"),
            ("the browser-approval poll", "async function bpPoll()"),
        ] {
            let body = script
                .split_once(marker)
                .map_or_else(|| panic!("{owner} is a named function"), |(_, rest)| rest);
            let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
            assert!(
                body.contains("rememberSession("),
                "{owner} stores the session"
            );
        }
    }

    #[test]
    fn a_click_is_attributed_to_the_button_and_not_to_whatever_is_inside_it() {
        // Reported from a phone and from Edge as "sometimes it works". A click landing on
        // the padding of a button has the button as its target; one landing on the label or
        // the icon inside it has the child as its target. The dispatcher read
        // `event.target.id`, so `admin-lock` — whose whole visible surface is a `<span>` —
        // did nothing when you clicked the word, and `nav-toggle` did nothing when you
        // clicked the icon.
        //
        // Nothing in this suite could see it. The page parsed, every id it addressed
        // existed, no id named two elements, every class was defined. The *text* was right
        // and the behaviour was wrong, which is the third time this file has produced that
        // combination.
        //
        // So the property is asserted where it lives: the dispatcher resolves a click to
        // the enclosing button before it looks at an id. Asserting instead that no
        // dispatched button contains a child element would be a list that goes stale the
        // next time somebody adds an icon.
        let script = page_script();
        let handler = script
            .split_once("document.addEventListener(\"click\"")
            .map(|(_, rest)| rest)
            .expect("the page dispatches clicks in one place");
        let handler = &handler[..handler.find("\n});").unwrap_or(handler.len())];

        assert!(
            handler.contains("closest(\"button\")"),
            "the dispatcher must resolve a click to its button: {handler}"
        );

        let reads_target_directly = handler
            .lines()
            .filter(|line| line.contains("event.target.id"))
            .filter(|line| !line.trim_start().starts_with("//"))
            .count();
        assert!(
            reads_target_directly <= 1,
            "only the fallback may read event.target.id, and it is reached when the click \
             was not on a button at all"
        );

        // And every button the dispatcher names is one the page can actually produce --
        // either written in the markup or written by a template. A typo here is a control
        // that silently does nothing, which is the same symptom by a different route.
        let mut dispatched = Vec::new();
        for (index, _) in handler.match_indices("id === \"") {
            let rest = &handler[index + "id === \"".len()..];
            if let Some(end) = rest.find('"') {
                dispatched.push(&rest[..end]);
            }
        }
        assert!(dispatched.len() >= 6, "found {dispatched:?}");
        for name in dispatched {
            // Written in the markup, or written by a template, or given an id in script --
            // the same three ways `every_element_the_script_reaches_for_exists_in_the_markup`
            // already accepts, because the terminal's take-over button is built that way.
            let exists = PAGE.contains(&format!("id=\"{name}\""))
                || script.contains(&format!(".id = \"{name}\""));
            assert!(
                exists,
                "the dispatcher answers to {name:?}, which nothing on the page creates"
            );
        }
    }

    #[test]
    fn the_theme_is_this_browsers_business_and_has_three_states() {
        // Three, not two: light, dark, and the default -- no choice made, so the operating
        // system decides. The third is the one that gets dropped, and dropping it means a
        // machine set to dark shows a light console until somebody finds the control.
        let style = page_style();
        assert!(
            style.contains("@media (prefers-color-scheme: dark)"),
            "the system's preference has to be honoured when nobody has chosen"
        );
        assert!(
            style.contains(":root:not([data-theme=\"light\"])"),
            "and the media query must be guarded, or an explicit light choice loses to a \
             dark operating system"
        );
        assert!(
            style.contains(":root[data-theme=\"dark\"]"),
            "and an explicit dark choice must win on a light one, which the media query \
             cannot express"
        );

        // Stored in the browser and not on the appliance. Two people on two screens must not
        // fight over it, and this console must not grow server state for a colour.
        let script = page_script();
        assert!(
            script.contains("localStorage.setItem(THEME_KEY")
                && script.contains("localStorage.removeItem(THEME_KEY"),
            "the choice is kept locally, and choosing 'system' removes it rather than \
             storing a third value that a future default could not change"
        );
        for value in ["\"system\"", "\"light\"", "\"dark\""] {
            assert!(
                PAGE.contains(&format!("<option value={value}")),
                "the selector offers {value}"
            );
        }
    }

    #[test]
    fn the_cards_that_fold_are_the_ones_a_running_appliance_rarely_opens() {
        // Six cards fold. Five start closed, and the terminal is the exception because it
        // is the one somebody opening this page has usually come for.
        for key in [
            "system", "settings", "shares", "update", "install", "terminal",
        ] {
            assert!(
                PAGE.contains(&format!("data-fold=\"{key}\"")),
                "the {key} card must carry a fold key, or its heading is not a control"
            );
        }
        let defaults = PAGE
            .split_once("const folded = new Set([")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(list, _)| list)
            .expect("the set of cards that start folded is declared in one place");
        for key in ["system", "settings", "shares", "update", "install"] {
            assert!(defaults.contains(key), "{key} must start folded");
        }
        assert!(
            !defaults.contains("terminal"),
            "the terminal must start open"
        );
    }

    #[test]
    fn a_folded_card_still_shows_what_went_wrong_inside_it() {
        // The failure this feature invents. A card that is closed cannot be clicked into,
        // so a report written into one lands where nobody can see it — and the code that
        // wrote it looks correct, because it did write it. Every path that puts a failure
        // or a running operation into a card has to open the card first.
        assert!(
            PAGE.contains("unfold(id);"),
            "sectionError must open the card it writes a failure into"
        );
        assert!(
            PAGE.contains("if (busy || phase === \"failed\") unfold(\"update\");"),
            "an update that is running or has failed must not be hidden"
        );
        assert!(
            PAGE.contains("if (busy || phase === \"failed\") unfold(\"install\");"),
            "nor an install"
        );
        assert!(
            PAGE.contains(".card[data-fold].folded > :not(h3):not(.bar) { display: none; }"),
            "folding must hide the body and keep the heading, which is the control"
        );
        assert!(
            PAGE.contains("[data-fold] > h3, [data-fold] > .bar"),
            "the heading and the terminal's bar are what a click lands on"
        );
    }

    #[test]
    fn opening_plex_leaves_this_page_where_it_is() {
        // The console is a page people leave open and come back to — a status console
        // that navigates away from itself is one they have to find again. And it is
        // served over TLS, so the link out of it is too.
        //
        // There are two of these now: the Overview's Plex card and the Plex view. So the
        // check is over *every* anchor whose text is "Open Plex" rather than over the first
        // one, which is the version that would have passed while a second link added later
        // handed a new tab an opener on this one.
        //
        // The scheme moved into `plexUrl` when the second link arrived, because two copies
        // of an absolute URL is exactly how two links come to disagree about it — and the
        // page's other rule is that it may hold only one absolute URL at all. So the TLS
        // half is asserted of the helper, and the anchors are asserted to use it.
        let anchors: Vec<&str> = PAGE
            .match_indices("Open Plex</a>")
            .filter_map(|(at, _)| {
                PAGE[..at]
                    .rfind("<a ")
                    .map(|start| &PAGE[start..at + "Open Plex</a>".len()])
            })
            .collect();
        assert_eq!(
            anchors.len(),
            2,
            "the Overview card and the Plex view each link to Plex"
        );

        for anchor in &anchors {
            assert!(
                anchor.contains("target=\"_blank\""),
                "Open Plex must open in a new tab: {anchor}"
            );
            assert!(
                anchor.contains("rel=\"noopener"),
                "and must not hand the new tab a handle on this one: {anchor}"
            );
            assert!(
                anchor.contains("href=\"${plexUrl("),
                "and must build its address through the one function that decides the \
                 scheme, rather than spelling it again: {anchor}"
            );
        }

        let helper = PAGE
            .split_once("function plexUrl(")
            .map(|(_, rest)| rest)
            .expect("plexUrl is a named function so that this can be checked");
        let body = &helper[..helper.find("\n}").unwrap_or(helper.len())];
        assert!(
            body.contains("https://"),
            "and that function must not drop out of TLS on the way: {body}"
        );
    }

    #[test]
    fn the_terminal_renders_what_the_shell_printed_and_not_less() {
        // The page destroyed sixty-one per cent of every session's output for the whole
        // life of this feature, and nothing here could see it: the tests assert what is in
        // the page, and the page was fine. The *behaviour* was wrong.
        //
        //     body.replace(/[^\n\b]\b/g, "")
        //
        // meant to say "drop a character a backspace erased". Inside a character class
        // `\b` is a backspace, so the first half says what it looks like; outside one it
        // is a **word boundary**, so the rule actually said "drop the last character of
        // every word". `total 4` came out `tota`, `root` came out `roo`, `drwxr-xr-x`
        // came out `drwxx`. Reported from a machine, reproduced by running this very
        // function over a captured session.
        //
        // So this test runs the page's own cleaner, under a real engine, over chunks
        // split the way a poll splits them -- mid escape sequence, and between the two
        // halves of a CRLF.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");

        let ansi = script
            .lines()
            .find(|line| line.starts_with("const ANSI = "))
            .expect("the control-character pattern is declared on one line");
        let start = script
            .find("function termClean(")
            .expect("the cleaner is a named function so that it can be tested");
        let end = script[start..]
            .find("\n}\n")
            .map(|at| start + at + "\n}".len())
            .expect("and ends at a brace in the first column");
        let cleaner = &script[start..end];

        // Every hazard in one session: a CRLF cut in half, an escape sequence cut in half,
        // a backspace erasing two characters, and a carriage return redrawing a line.
        let chunks = [
            "total 4\r",
            "\ndrwxr-xr-x   10 root     root  .\r\n",
            "\u{1b}[1;3",
            "4mbin\u{1b}[m -> usr/bin\r\n",
            "abc\u{8}\u{8}d\r\n",
            "50%\r100%\r\n",
        ];
        let expected = "total 4\ndrwxr-xr-x   10 root     root  .\nbin -> usr/bin\nad\n100%\n";

        let Some(engine) = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            println!(
                "skip: the terminal cleaner was not run -- no node, deno or qjs on this \
                 host. Install one, or output corruption will only be found by reading a \
                 shell's output on an appliance."
            );
            return;
        };

        let driver = format!(
            "{ansi}\n{cleaner}\n\
             const chunks = {};\n\
             let pending = \"\", body = \"\";\n\
             for (const chunk of chunks) {{\n\
             \x20 const done = termClean(pending, chunk);\n\
             \x20 body += done[0];\n\
             \x20 pending = done[1];\n\
             }}\n\
             process.stdout.write(JSON.stringify(body));\n",
            serde_json::to_string(&chunks).expect("chunks encode")
        );

        // Named for this test: Rust runs tests as threads in one process, so a fixed path
        // is one test deleting what another is reading.
        let file = std::env::temp_dir().join("plexos-terminal-cleaner-test.js");
        std::fs::write(&file, driver).expect("scratch is writable");
        let run = std::process::Command::new(engine)
            .arg(&file)
            .output()
            .expect("the engine runs");
        let _ = std::fs::remove_file(&file);

        assert!(
            run.status.success(),
            "the cleaner did not run:\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let rendered: String =
            serde_json::from_slice(&run.stdout).expect("the driver prints a JSON string");
        assert_eq!(
            rendered, expected,
            "the terminal does not render what the shell printed"
        );
    }

    #[test]
    fn the_sparkline_spans_the_tile_whatever_it_has_to_draw() {
        // Same shape as the terminal cleaner above, and for the same reason: the page was
        // fine and the *drawing* was wrong. `metricSpark` shipped three times before it was
        // right, and the first two were only found by rendering a picture and looking at it.
        //
        //   1. x scaled by the ring's capacity, so four samples drew a line five per cent of
        //      the tile wide, tucked in the left corner.
        //   2. "fixed" by anchoring at the right, which moved the same stub to the other
        //      corner.
        //   3. spread over the points there are, which is what this asserts.
        //
        // No assertion about the page's text could see any of it, and neither could
        // `node --check`: every version was valid JavaScript that produced valid SVG.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");

        let start = script
            .find("const SPARK_POINTS")
            .expect("the ring size is declared before the functions that use it");
        let end = script
            .find("function metricTile(")
            .expect("and the drawing functions end where the tile begins");
        let slice = &script[start..end];

        let Some(engine) = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            println!(
                "skip: the sparkline was not drawn -- no node, deno or qjs on this host. \
                 Install one, or a sparkline that draws a stub in the corner will only be \
                 found by looking at a rendered page."
            );
            return;
        };

        let driver = format!(
            "{slice}\n\
             const out = {{}};\n\
             for (const n of [1, 2, 3, 8, 200]) {{\n\
             \x20 const ring = [];\n\
             \x20 for (let i = 0; i < n; i++) pushSample(ring, 10 + (i % 7) * 12);\n\
             \x20 out[n] = {{ svg: metricSpark(ring, 100), kept: ring.length }};\n\
             }}\n\
             process.stdout.write(JSON.stringify(out));\n"
        );

        // Named for this test, because Rust runs tests as threads in one process and a fixed
        // path is one test deleting what another is reading.
        let file = std::env::temp_dir().join("plexos-sparkline-draw-test.js");
        std::fs::write(&file, driver).expect("scratch is writable");
        let run = std::process::Command::new(engine)
            .arg(&file)
            .output()
            .expect("the engine runs");
        let _ = std::fs::remove_file(&file);
        assert!(
            run.status.success(),
            "the sparkline did not draw:\n{}",
            String::from_utf8_lossy(&run.stderr)
        );

        let drawn: serde_json::Value =
            serde_json::from_slice(&run.stdout).expect("the driver prints JSON");

        for few in ["1", "2"] {
            assert_eq!(
                drawn[few]["svg"].as_str(),
                Some(""),
                "a line needs three points to be a line rather than a dash that reads as a \
                 rendering fault"
            );
        }

        for many in ["3", "8", "200"] {
            let svg = drawn[many]["svg"].as_str().expect("an SVG");
            assert!(
                svg.contains("points=\"0.00,"),
                "{many} points must start at the left edge: {svg}"
            );
            assert!(
                svg.contains(" 100.00,"),
                "and reach the right one, so the trend fills the tile instead of drawing a \
                 stub in a corner: {svg}"
            );
            assert!(
                svg.contains("class=\"now\""),
                "the newest segment is marked, and by a stroke rather than a shape: a circle \
                 here is squashed by the non-uniform scale and clipped at the edge: {svg}"
            );
            assert!(
                !svg.contains("<circle"),
                "and specifically not by a circle, which was tried and looked at: {svg}"
            );
        }

        assert_eq!(
            drawn["200"]["kept"].as_u64(),
            Some(60),
            "the ring is bounded, or a tab left open all day grows one point per two seconds"
        );
    }

    #[test]
    fn the_wireless_route_never_answers_with_the_key() {
        // This route is readable by anyone on the LAN, because every GET here is. The
        // pre-shared key is on /var at 0600 precisely so that it is not; serialising the
        // stored network wholesale would have handed it out on an unauthenticated read,
        // and it would have looked exactly like every other report on the page.
        let response = respond_test(&get("/api/wifi"), &Fixture::new());
        assert_eq!(response.status, 200);
        let body = String::from_utf8_lossy(&response.body);
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("parsable");
        assert!(
            parsed.get("psk").is_none(),
            "the key must not be in the report: {body}"
        );
        assert!(!body.contains("psk"), "nor under any other name: {body}");
        assert!(
            parsed.get("configured").is_some(),
            "the *name* of the remembered network is fine, and is what a page needs"
        );
    }

    #[test]
    fn a_wireless_action_nobody_offers_says_which_ones_exist() {
        let mut request = get("/api/wifi");
        request.method = "POST".to_owned();
        request.body = br#"{"action":"levitate"}"#.to_vec();
        let response = respond_test(&request, &Fixture::new());
        assert_eq!(response.status, 400);
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("scan"), "{body}");
        assert!(body.contains("join"), "{body}");
        assert!(body.contains("forget"), "{body}");
    }

    #[test]
    fn joining_nothing_in_particular_is_refused_before_the_radio_is_touched() {
        // A join with no name would otherwise claim the radio, spawn a thread, and fail
        // there -- so the refusal would arrive as a job state rather than as an answer to
        // the request that was wrong.
        let mut request = get("/api/wifi");
        request.method = "POST".to_owned();
        request.body = br#"{"action":"join"}"#.to_vec();
        let response = respond_test(&request, &Fixture::new());
        assert_eq!(response.status, 400);
        assert!(String::from_utf8_lossy(&response.body).contains("ssid"));

        request.body = br#"{"action":"join","ssid":""}"#.to_vec();
        let response = respond_test(&request, &Fixture::new());
        assert_eq!(response.status, 400);
        assert!(
            String::from_utf8_lossy(&response.body).contains("hidden network still has"),
            "and says why an empty name is not the way to join a hidden one"
        );
    }

    #[test]
    fn the_terminal_can_be_given_a_window_of_its_own() {
        // A shell in a card on a status page is as tall as the card. In a window it is as
        // tall as the window, and the ResizeObserver already tells the shell when that
        // changes -- so dragging the window resizes the terminal.
        //
        // A query rather than a route, which is why the redirect had to stop dropping the
        // query string: `http://<address>/?view=terminal` has to survive the bounce to
        // HTTPS, or the popup lands on the console page instead.
        assert!(
            PAGE.contains("\"view\") === \"terminal\""),
            "the popup is the same page, told what it is by its query"
        );
        assert!(
            PAGE.contains("id=\"term-window\""),
            "and there is a control that opens it"
        );
        assert!(
            PAGE.contains("\"plexos-terminal\""),
            "the window is named, so a second click focuses the one already open rather \
             than opening another onto a shell that allows one session"
        );
        assert!(
            PAGE.contains(".solo > :not(.wide):not(#token-card) { display: none; }"),
            "the rest of the page is hidden and not removed -- listeners are bound to it \
             before any of this runs, and a missing element there is a dead script"
        );
        assert!(
            PAGE.contains("if (SOLO) {"),
            "and the popup polls nothing: it has no status card to redraw"
        );
    }

    #[test]
    fn backspace_is_never_spelled_as_a_word_boundary() {
        // The guard for the defect above, in the form somebody would reintroduce it: `\b`
        // reads as "backspace" and is one only inside a character class. This is cheap and
        // it is specific, which is what a guard against a re-typed mistake has to be.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");
        // Comments are skipped, because the explanation of a defect has to be allowed to
        // quote it -- and the first version of this test tripped over its own description
        // of the bug, which is a check that cannot be lived with.
        let code: String = script
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("]\\b"),
            "a `\\b` outside a character class is a word boundary, not a backspace"
        );
        assert!(
            script.contains("[^\\n\\x08]\\x08"),
            "backspace erasure must be spelled with \\x08, which means one thing only"
        );
    }

    #[test]
    fn the_terminal_reports_the_size_it_negotiated() {
        // The invisible half of every complaint about this terminal. A shell wrapping at
        // the wrong width looks like a rendering fault, and there was no way to see what
        // the shell had been told — so the status line says it now.
        assert!(
            PAGE.contains("async function termResize()"),
            "a window that changes size must tell the shell, or it keeps wrapping at the \
             width it was opened with"
        );
        assert!(
            PAGE.contains("action: \"resize\""),
            "and must use the route the console already serves for it"
        );
        assert!(
            PAGE.contains("if (!out || !out.clientWidth) return { rows: 24, columns: 80 };"),
            "a folded or undrawn screen measures zero, and the floor below turns that into \
             a shell told it has twenty columns"
        );
    }

    #[test]
    fn the_script_declares_no_function_twice() {
        // The other half of the same defect, and the half that made it dangerous. There
        // were two `startInstall` functions in this one script: Plex's and the disk
        // installer's. A function declaration is not an error to repeat -- the later one
        // silently replaces the earlier -- so the Install Plex button was wired to the code
        // that erases a disk. The duplicate `const` that blanked this page once *is* a parse
        // error and `the_pages_script_parses` catches it; a duplicate `function` is not, and
        // nothing catches it but this.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");

        let mut names: Vec<&str> = script
            .lines()
            .filter_map(|line| {
                let line = line.trim_start();
                let rest = line
                    .strip_prefix("async function ")
                    .or_else(|| line.strip_prefix("function "))?;
                rest.split_once('(').map(|(name, _)| name.trim())
            })
            .collect();
        names.sort_unstable();
        let duplicated: Vec<&str> = names
            .windows(2)
            .filter(|pair| pair[0] == pair[1])
            .map(|pair| pair[0])
            .collect();
        assert!(
            duplicated.is_empty(),
            "these functions are declared more than once, so the last one wins and every \
             earlier caller now runs code it was never pointed at: {duplicated:?}"
        );
    }

    #[test]
    fn the_page_leads_a_new_appliance_through_setup_and_then_stops() {
        // A machine five minutes old used to show exactly the page one running for a year
        // showed. The section has to exist, has to be driven by the endpoint rather than by
        // anything the browser remembers, and has to disappear on its own -- a banner that
        // must be dismissed is one people dismiss before reading.
        assert!(PAGE.contains("\"/api/setup\""));
        assert!(PAGE.contains("This appliance has just been installed"));
        assert!(
            PAGE.contains("id=\"setup\""),
            "the section has to exist in the markup, not only in the script that fills it"
        );
        assert!(
            PAGE.contains("report.complete"),
            "the section's visibility comes from the machine's own state"
        );
    }

    #[test]
    fn the_page_says_what_installing_destroys_before_it_offers_to_do_it() {
        // The only control on this page that erases data which was never MediaLith's. The
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
                &Services {
                    provision: std::sync::Arc::clone(&job),
                    ..Services::new()
                },
            );
            assert_ne!(response.status, 404, "{method} {path}");
        }
    }

    #[test]
    fn the_activity_card_polls_a_route_that_exists() {
        assert!(
            PAGE.contains("\"/api/metrics\""),
            "the page must fetch the route respond() serves"
        );

        let response = respond_test(&get("/api/metrics"), &Fixture::new());
        assert_eq!(response.status, 200);
        let body = String::from_utf8(response.body).expect("utf-8");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

        // The fields the page reads by name. A rename here is a card full of "—" on a
        // machine whose route answers perfectly, which is the failure mode that has cost
        // this project the most time to spot.
        for field in [
            "cpu",
            "memory",
            "plex",
            "storage",
            "network",
            "disks",
            "temperatures",
            "notes",
            "window_ms",
            "uptime_seconds",
            "load",
        ] {
            assert!(
                parsed.get(field).is_some(),
                "the page reads `{field}` off this reply: {body}"
            );
        }
        assert!(
            parsed["cpu"].get("busy_percent").is_some(),
            "including the nested ones"
        );
    }

    #[test]
    fn an_idle_graphics_clock_is_only_called_idle_when_the_gpu_is_well() {
        // `0 MHz` is the normal state of this appliance -- a part with nothing to do parks
        // its clock -- and printed plainly it reads as a broken GPU. Saying "Idle" instead
        // fixes that and introduces a way to hide the one failure this whole project exists
        // to make loud, because `0 MHz` on a machine whose transcoding stack is degraded or
        // unavailable is exactly the reading somebody should be staring at.
        //
        // So the condition is the feature, and it is checked by running it rather than by
        // reading it: the interesting cases are the ones where the answer must be *no*.
        let script = page_script();
        let start = script
            .find("function gpuIsIdle(")
            .expect("the rule is a named function so that it can be run");
        let end = script[start..]
            .find("\n}\n")
            .map(|at| start + at + "\n}".len())
            .expect("and ends at a brace in the first column");

        let Some(engine) = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            println!(
                "skip: the idle rule was not run -- no node, deno or qjs on this host. \
                 Install one, or a GPU that has failed will only be found by somebody \
                 noticing that a broken machine says it is idle."
            );
            return;
        };

        // health, clock, and whether "Idle" may be shown.
        let cases: [(&str, &str, bool); 7] = [
            ("\"ready\"", "0", true),
            ("\"degraded\"", "0", false),
            ("\"unavailable\"", "0", false),
            ("undefined", "0", false),
            ("\"unknown\"", "0", false),
            ("\"ready\"", "450", false),
            ("\"ready\"", "null", false),
        ];
        let calls = cases
            .iter()
            .map(|(health, clock, _)| format!("gpuIsIdle({health}, {clock})"))
            .collect::<Vec<_>>()
            .join(", ");
        let program = format!(
            "{}\nconsole.log([{calls}].join(\",\"));",
            &script[start..end]
        );

        let output = std::process::Command::new(engine)
            .args(["-e", &program])
            .output()
            .expect("the engine runs");
        assert!(
            output.status.success(),
            "the idle rule did not run: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let got = String::from_utf8_lossy(&output.stdout);
        let answers: Vec<&str> = got.trim().split(',').collect();
        assert_eq!(answers.len(), cases.len(), "one answer per case: {got}");
        for ((health, clock, want), got) in cases.iter().zip(answers) {
            assert_eq!(
                got,
                if *want { "true" } else { "false" },
                "gpuIsIdle({health}, {clock}) must be {want}: a degraded or unavailable \
                 stack reporting zero is the reading that matters, and calling it idle \
                 hides it"
            );
        }
    }

    #[test]
    fn the_overview_puts_activity_beside_a_rail_that_has_room_to_grow() {
        // The rail is where Now Playing and Recent Events go when there is something real to
        // put in them, and it is sized for that now so their arrival is a card rather than a
        // re-layout. This asserts the arrangement exists, because the alternative -- three
        // sections stacked down one column -- is what it replaced and is one deleted
        // wrapper away.
        assert!(
            PAGE.contains("class=\"ovgrid\" id=\"ovgrid\""),
            "the Overview's composition below the summary row is one grid"
        );
        for id in ["metrics", "main", "snapshot"] {
            assert!(
                PAGE.contains(&format!("id=\"{id}\"")),
                "{id} is one of the three regions that grid places"
            );
        }

        let style = page_style();
        assert!(
            style.contains(".ovgrid {"),
            "and the grid is laid out in the stylesheet rather than inline"
        );
        // The failure case: a list of findings and their remedies in a one-third column is a
        // column of hyphens, so the health panel leaves the rail when it has something to
        // say. Driven by a class the render sets from the data it just drew.
        assert!(
            style.contains(".ovgrid.alert > #main"),
            "a health panel with findings in it must be able to take the full width"
        );
        assert!(
            page_script().contains("classList.toggle(\"alert\""),
            "and that state comes from the same data the panel was built from"
        );
    }

    #[test]
    fn every_severity_a_meter_can_reach_is_one_the_stylesheet_paints() {
        // A meter's whole job is that 96% does not look like 6%, and for the entire life of
        // the activity card it did. `metricLevel` returned `warn-metricLevel` and
        // `bad-metricLevel` while the stylesheet defined `.meter.warn-level` and
        // `.meter.bad-level`, so no meter ever left the accent colour: a /var about to fill
        // drew exactly like an empty one. The rename that produced it was applied to the file
        // rather than to the identifier and rewrote the two string literals along with the
        // function name -- trap one in CLAUDE.md, in the form where the result still parses,
        // still renders, and is wrong only in a colour nobody had a reference for.
        //
        // Nothing that reads the page as text could catch it: both halves were internally
        // consistent. So the class names are taken from the function *by running it*, and
        // each one is looked up in the stylesheet.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");
        let style = PAGE
            .split_once("<style>")
            .and_then(|(_, rest)| rest.split_once("</style>"))
            .map(|(body, _)| body)
            .expect("the page has one style block");

        let start = script
            .find("function metricLevel(")
            .expect("the severity of a reading is decided by a named function");
        let end = script[start..]
            .find("\n}\n")
            .map(|at| start + at + "\n}".len())
            .expect("and ends at a brace in the first column");

        let Some(engine) = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            println!(
                "skip: the meter's severity classes were not compared against the stylesheet \
                 -- no node, deno or qjs on this host. Install one, or a meter that never \
                 leaves the accent colour will only be found by somebody noticing that a full \
                 disk looks like an empty one."
            );
            return;
        };

        // Below the warning, between the two, and above the failure threshold: the three
        // answers this function has, asked for with the defaults the meters use.
        let program = format!(
            "{}\nconsole.log([10, 80, 97].map(p => metricLevel(p, 70, 90)).join(\"\\n\"));",
            &script[start..end]
        );
        let output = std::process::Command::new(engine)
            .args(["-e", &program])
            .output()
            .expect("the engine runs");
        assert!(
            output.status.success(),
            "the page's severity function did not run: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let got = String::from_utf8_lossy(&output.stdout);
        let classes: Vec<&str> = got.lines().map(str::trim).collect();
        assert_eq!(
            classes.len(),
            3,
            "three readings, three answers: {classes:?}"
        );
        assert_eq!(classes[0], "", "an unremarkable reading earns no class");
        for class in &classes[1..] {
            assert!(
                !class.is_empty(),
                "a reading over a threshold must earn a class: {classes:?}"
            );
            assert!(
                style.contains(&format!(".meter.{class} ")),
                "the script puts `{class}` on a meter and the stylesheet paints no such \
                 thing, so that meter draws in the accent however bad the reading is. The \
                 stylesheet is the definition; the script is what changes."
            );
        }
    }

    #[test]
    fn the_process_list_cannot_be_read_without_a_credential() {
        // The whole reason this is a second route. A GET on this console needs nothing, by
        // design -- somebody diagnosing a machine that will not boot should not have to find
        // a token first. A list of what is running with its command lines is not that: it is
        // closer to what the terminal exposes, and the terminal is all POST for exactly this
        // reason. The gate in http::route is method-based, so being a POST *is* the
        // protection, and a GET arriving here must find nothing.
        assert_eq!(
            respond_test(&get("/api/metrics/processes"), &Fixture::new()).status,
            404,
            "a GET must not answer with the process list, or the token gate is bypassed by \
             asking politely"
        );

        let post = Request {
            method: "POST".to_owned(),
            path: "/api/metrics/processes".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert_eq!(
            respond_test(&post, &Fixture::new()).status,
            200,
            "and the POST is the route that does exist"
        );
    }

    #[test]
    fn the_page_asks_for_the_process_list_with_the_token() {
        // The companion to the test above, from the page's side: a POST route reached
        // without the header is a 403 and a section that silently does nothing.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("one script block");
        let request = script
            .split_once("PROCESSES_ENDPOINT, {")
            .map(|(_, rest)| rest)
            .expect("the page posts to the processes route");
        let call = &request[..request.find("})").unwrap_or(request.len())];

        assert!(call.contains("method: \"POST\""), "as a POST: {call}");
        assert!(
            call.contains("Authorization"),
            "and carrying the device token: {call}"
        );
    }

    #[test]
    fn the_page_calls_the_product_by_its_name() {
        // The rename is only done when the artefact says so. This asserts the three places
        // a person actually reads it — the tab, the header, and the terminal window — and
        // then that the old name appears nowhere in the page at all.
        assert!(PAGE.contains("<title>MediaLith</title>"), "the browser tab");
        assert!(
            PAGE.contains(r#"<h1 id="product">MediaLith</h1>"#),
            "the header"
        );
        assert!(
            PAGE.contains(r#"document.title = "MediaLith Terminal""#),
            "the terminal window"
        );

        // Deliberately the whole file, comments included: a comment that still describes
        // this as PlexOS is telling the next reader something untrue about the product.
        // Internal *identifiers* are lower-case `plexos` and are not what this looks for.
        assert!(
            !PAGE.contains("PlexOS"),
            "the page still calls the product PlexOS somewhere"
        );
    }

    #[test]
    fn the_page_says_what_it_is_told_rather_than_assuming_the_product_name() {
        // The header shows `PRETTY_NAME` from the machine's own os-release, and falls back
        // to the product name only when there is none. That matters more after a rename
        // than before it: an appliance running an older release must be described by the
        // name *it* reports, not by the name this page was built with, or the console would
        // tell somebody they are running MediaLith while the machine is not.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("one script block");
        assert!(
            script.contains("p.name || \"MediaLith\""),
            "the fallback is only reached when os-release said nothing"
        );
    }

    #[test]
    fn what_somebody_is_watching_cannot_be_read_without_a_credential() {
        // The same argument as the process list, and the stronger case of the two. The open
        // `GET` exists so a *broken* machine can be diagnosed; a film title, a username and a
        // device name are not diagnostics, and a `GET` here would leave a household's viewing
        // readable by anything on the LAN for as long as the appliance runs.
        //
        // Being a POST *is* the protection, because the gate in `http::route` is method-based
        // — so a GET arriving here has to find nothing at all.
        assert_eq!(
            respond_test(&get("/api/plex/sessions"), &Fixture::new()).status,
            404,
            "a GET must not answer with what is playing, or the gate is bypassed by asking \
             politely"
        );

        let post = Request {
            method: "POST".to_owned(),
            path: "/api/plex/sessions".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let answered = respond_test(&post, &Fixture::new());
        assert_eq!(
            answered.status, 200,
            "and the POST is the route that exists"
        );

        // On a build host with no Plex, the answer is a state with a remedy rather than an
        // error — the whole point of the failure model. Whatever it says, it says nothing
        // about anybody: there are no sessions to describe.
        let body = String::from_utf8_lossy(&answered.body);
        assert!(
            body.contains("\"available\":false") || body.contains("\"sessions\":[]"),
            "an appliance with no Plex has nothing to report: {body}"
        );
        assert!(body.contains("Remedy:"), "and it names one: {body}");
    }

    #[test]
    fn the_page_never_asks_what_is_playing_without_the_token() {
        // From the page's side, and this is the half that matters for privacy: the rule is
        // not "fetch it and hide it", it is "do not ask". A page that downloaded the titles
        // and then declined to draw them would have put them in a browser that was never
        // entitled to them.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("one script block");

        let request = script
            .split_once("PLEX_SESSIONS_ENDPOINT, {")
            .map(|(_, rest)| rest)
            .expect("the page posts to the sessions route");
        let call = &request[..request.find("})").unwrap_or(request.len())];
        assert!(call.contains("method: \"POST\""), "as a POST: {call}");
        assert!(
            call.contains("Authorization"),
            "and carrying the device token: {call}"
        );

        // And the guard in front of it: the poll returns before the fetch when the tab holds
        // no token. Asserted on the shape rather than the wording, because this is the one
        // control on the page whose absence is invisible -- everything would still work, and
        // an unauthenticated browser would be told what somebody is watching.
        let poll = script
            .split_once("async function plexActivityTick()")
            .map(|(_, rest)| rest)
            .expect("the activity poll exists");
        let head = &poll[..poll.find("PLEX_SESSIONS_ENDPOINT").unwrap_or(poll.len())];
        assert!(
            head.contains("const value = credential();") && head.contains("if (!value)"),
            "the poll must decide on the credential before it reaches the fetch: {head}"
        );
    }

    #[test]
    fn the_progress_clock_advances_only_while_plex_says_it_is_playing() {
        // The one place this console deliberately shows something the appliance did not say
        // in so many words: between polls, the bar and the clock move on a local timer, or
        // the numbers sit still for three seconds and then jump three, which reads as a page
        // that has come unstuck from the machine.
        //
        // What makes that honest is entirely in this function, so it is a function and this
        // runs it — the lesson from `termClean`, which was wrong for the whole life of the
        // feature because the page's *text* was fine and its behaviour was not.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");

        let start = script
            .find("function npAdvance(")
            .expect("the page advances the clock somewhere");
        let end = script[start..]
            .find("function npTick(")
            .expect("and npTick is what uses it")
            + start;
        let slice = &script[start..end];

        let Some(engine) = ["node", "deno", "qjs"].into_iter().find(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            println!(
                "skip: the progress clock was not run -- no node, deno or qjs on this host. \
                 Install one, or a clock that counts through a pause will only be found by \
                 watching a paused film advance on the page."
            );
            return;
        };

        // Ten minutes in, of a two-hour film, five seconds after the appliance answered.
        let driver = format!(
            "{slice}\n\
             const film = {{ position_ms: 600000, duration_ms: 7200000 }};\n\
             const out = {{\n\
             \x20 playing: npAdvance({{ ...film, state: 'playing' }}, 5000),\n\
             \x20 paused: npAdvance({{ ...film, state: 'paused' }}, 5000),\n\
             \x20 buffering: npAdvance({{ ...film, state: 'buffering' }}, 5000),\n\
             \x20 unstated: npAdvance(film, 5000),\n\
             \x20 past_the_end: npAdvance({{ position_ms: 7199000, duration_ms: 7200000, \
             state: 'playing' }}, 60000),\n\
             \x20 no_duration: npAdvance({{ position_ms: 600000, state: 'playing' }}, 5000),\n\
             \x20 backwards: npAdvance({{ ...film, state: 'playing' }}, -9000),\n\
             }};\n\
             console.log(JSON.stringify(out));"
        );

        let output = std::process::Command::new(engine)
            .arg("-e")
            .arg(&driver)
            .output()
            .expect("the engine runs");
        assert!(
            output.status.success(),
            "{engine} refused the page's own function: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let answer: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("the driver prints JSON");

        assert_eq!(
            answer["playing"], 605_000,
            "five seconds of wall clock is five seconds of film"
        );
        for still in ["paused", "buffering", "unstated"] {
            assert_eq!(
                answer[still], 600_000,
                "{still}: a film that is not playing does not advance, and inventing progress \
                 for one is the whole thing this must never do"
            );
        }
        assert_eq!(
            answer["past_the_end"], 7_200_000,
            "a bar wider than its track breaks the card, and Plex does report a position past \
             the end as an item finishes"
        );
        assert_eq!(
            answer["no_duration"], 0,
            "with nothing to be a fraction of, there is no position to draw"
        );
        assert_eq!(
            answer["backwards"], 600_000,
            "a clock that went backwards would be a machine whose clock had, and this one is \
             the browser's"
        );
    }

    #[test]
    fn every_field_the_page_reads_off_a_session_is_one_the_server_sends() {
        // The fourth question about this page, asked about a fifth pair of files. Three tests
        // already check that the script parses, that every id it addresses exists and that
        // every class it draws with is defined — and all three would pass while the page read
        // `session.player_title` from a server that sends `player`. The symptom is a card
        // that renders perfectly with one line quietly missing from it.
        //
        // So: build the document the route actually serialises, and look up every field the
        // page reaches for. `session`, `video` and `audio` are names this page uses nowhere
        // else, which is what makes the extraction exact rather than a guess.
        let sample = crate::plexactivity::Report {
            available: true,
            state: crate::plexactivity::State::Playing,
            detail: crate::plexactivity::State::Playing.detail().to_owned(),
            active: 1,
            sessions: vec![crate::plexactivity::Session {
                id: Some("1".to_owned()),
                rating_key: Some("118".to_owned()),
                kind: Some("movie".to_owned()),
                title: Some("Test Feature".to_owned()),
                series: Some("Test Series".to_owned()),
                episode: Some("S02E05".to_owned()),
                user: Some("Sebastian".to_owned()),
                player: Some("Living Room TV".to_owned()),
                platform: Some("tvOS".to_owned()),
                product: Some("Plex for Apple TV".to_owned()),
                state: Some("playing".to_owned()),
                local: Some(true),
                position_ms: Some(5_538_000),
                duration_ms: Some(10_143_000),
                decision: crate::plexactivity::Decision::Transcode,
                source_bitrate_kbps: Some(24_399),
                stream_bitrate_kbps: Some(2798),
                transcode: Some(crate::plexactivity::Transcode {
                    progress: Some(1.3),
                    speed: Some(0.0),
                    throttled: Some(true),
                    error: Some(false),
                }),
                video: crate::plexactivity::Video {
                    decision: crate::plexactivity::Decision::Transcode,
                    source_codec: Some("hevc".to_owned()),
                    source_resolution: Some("4K".to_owned()),
                    source_hdr: Some("HDR10".to_owned()),
                    target_codec: Some("h264".to_owned()),
                    target_resolution: Some("1080p".to_owned()),
                    hardware: Some(true),
                    hardware_detail: Some("Intel (VA API)".to_owned()),
                    full_pipeline: Some(true),
                },
                audio: crate::plexactivity::Audio {
                    decision: crate::plexactivity::Decision::Transcode,
                    source_codec: Some("truehd".to_owned()),
                    source_channels: Some(8),
                    target_codec: Some("aac".to_owned()),
                    target_channels: Some(2),
                },
            }],
        };

        let document: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&sample).expect("serialises"))
                .expect("is JSON");
        let session = &document["sessions"][0];

        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("one script block");

        let mut checked = 0;
        for (holder, node) in [
            ("session", session),
            ("video", &session["video"]),
            ("audio", &session["audio"]),
        ] {
            let prefix = format!("{holder}.");
            for occurrence in script.match_indices(&prefix) {
                let rest = &script[occurrence.0 + prefix.len()..];
                let field: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '_')
                    .collect();
                // `session.` followed by nothing lowercase is a sentence in a comment, not a
                // field access.
                if field.is_empty() {
                    continue;
                }
                assert!(
                    node.get(&field).is_some(),
                    "the page reads {holder}.{field}, which the server does not send. \
                     What it does send: {node}"
                );
                checked += 1;
            }
        }
        assert!(
            checked > 20,
            "only {checked} field accesses found — the extraction has stopped working, which \
             would make this test pass by looking at nothing"
        );
    }

    #[test]
    fn nothing_a_person_opened_lives_inside_the_region_on_a_timer() {
        // Reported from the machine: expanding the process list or the notes worked for up to
        // two seconds and then shut itself. Both were rendered *into* `#metrics-body`, which
        // the poll replaces wholesale twice a second — so the element holding the open state
        // was destroyed and rebuilt closed. It reads as a control that refuses to stay open.
        //
        // The fix is structural rather than a saved-and-restored flag: the two stateful parts
        // are written once, in the markup, as siblings of the redrawn region. This asserts
        // that arrangement, because putting either one back inside is a one-line mistake that
        // no other test here can see.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");

        for id in [
            "procs-panel",
            "procs",
            "metrics-notes",
            "metrics-notes-list",
            // Added by the redesign, and the third thing to need this arrangement. The
            // per-core meters, the interface counters, the disk rates and the thermal zones
            // moved off the front of the card into a `<details>` -- so the card leads with
            // the five figures somebody opens it for instead of eleven at one weight. That
            // disclosure has exactly the state the two above have, and putting it inside the
            // redrawn region would have reproduced their defect a third time.
            "metrics-detail",
            "metrics-detail-body",
        ] {
            assert!(
                PAGE.contains(&format!("id=\"{id}\"")),
                "{id} must exist in the markup rather than be generated by the poll"
            );
        }

        // What the poll writes, from the assignment to the end of that template literal.
        let written = script
            .split_once("document.getElementById(\"metrics-body\").innerHTML = `")
            .map(|(_, rest)| rest)
            .expect("the poll redraws the body");
        let written = &written[..written.find("`;").unwrap_or(written.len())];

        for forbidden in [
            "id=\"procs",
            "<details",
            "metrics-notes",
            "metrics-detail",
            // The tables themselves. They are the *contents* of the disclosure now, written
            // into it only when they have changed -- so their appearance inside the poll's
            // own template would mean the move had been undone.
            "<table",
        ] {
            assert!(
                !written.contains(forbidden),
                "the redrawn region must not contain {forbidden}: anything with state a \
                 person set is destroyed twice a second there"
            );
        }
    }

    #[test]
    fn every_view_that_can_be_opened_can_be_shut_again() {
        // Asked for as a rule rather than as a bug: "every section that can be expanded must
        // also be able to go back to collapsed". Two on this page could not.
        //
        // The process list, once fetched, had no control that put it away — and it is long.
        // The network diagnosis was worse: its output sat under the network card for the rest
        // of the session, and because that card is rebuilt from its template every ten
        // seconds, the state had to move into a variable rather than the element.
        //
        // The folding cards and the notes `<details>` already satisfy this, which is why they
        // are not listed here; what this guards is the two that were added without it.
        let script = PAGE
            .split_once("<script>")
            .and_then(|(_, rest)| rest.rsplit_once("</script>"))
            .map(|(body, _)| body)
            .expect("the page has one script block");

        assert!(
            PAGE.contains("Show Processes") && PAGE.contains("Hide Processes"),
            "the process list's control has to say both things, or it only ever opens"
        );
        assert!(
            script.contains("function showProcesses("),
            "and hiding is a function rather than a relabel, so both directions exist"
        );

        assert!(
            PAGE.contains("id=\"netdiag-hide\""),
            "the network diagnosis needs a way back to a page without it"
        );
        assert!(
            script.contains("netdiagShown"),
            "and it must be a variable, because that card is rebuilt every status poll and an \
             element's own state does not survive being replaced"
        );
    }

    #[test]
    fn the_header_carries_what_is_wanted_without_scrolling() {
        // The page is several screens long — the activity card, the terminal and the installer
        // each take one — so the header is sticky and holds the three things worth having to
        // hand: what this machine is, how long it has been up, and how to stop it. The power
        // controls moved out of a card at the very bottom, which on a long page meant
        // scrolling past everything to restart a machine.
        let head = PAGE
            .split_once("<header>")
            .and_then(|(_, rest)| rest.split_once("</header>"))
            .map(|(body, _)| body)
            .expect("the page has one header");

        for id in ["product", "version", "uptime", "restart", "shutdown"] {
            assert!(
                head.contains(&format!("id=\"{id}\"")),
                "{id} belongs in the header: {head}"
            );
        }

        let style = PAGE
            .split_once("<style>")
            .and_then(|(_, rest)| rest.split_once("</style>"))
            .map(|(body, _)| body)
            .expect("the page has one style block");
        assert!(
            style.contains("position: sticky"),
            "and the header sticks, or none of the above is reachable from further down"
        );

        // The two controls that end the session they are pressed in are marked as such, since
        // they now sit a centimetre from the pointer at all times.
        assert!(
            head.matches("danger").count() >= 2,
            "both power controls are marked dangerous: {head}"
        );
        assert!(
            !PAGE.contains("class=\"card power\""),
            "and the card they came from is gone rather than left empty"
        );
    }

    #[test]
    fn buttons_are_styled_without_naming_where_they_are() {
        // This has been narrowed by the same mistake twice. First the base rules were
        // `.plex button`, `.form button` and `.power button`, so the network card — a plain
        // `<div class="card">` — matched none of them and its button had *no* styling: a raw
        // operating-system control in a designed page, which shipped and was reported by
        // somebody looking at it. That was fixed to `.card button`, which then missed the next
        // button added outside a card, in the header.
        //
        // So the assertion is not about which container is named. It is that **none is**: a
        // rule about how a button looks should not know where buttons are.
        let style = PAGE
            .split_once("<style>")
            .and_then(|(_, rest)| rest.split_once("</style>"))
            .map(|(body, _)| body)
            .expect("the page has one style block");

        assert!(
            style.contains("\n  button {"),
            "the button base must be on the element, so a button added anywhere is styled"
        );
        for scoped in [
            ".plex button {",
            ".form button {",
            ".power button {",
            ".card button {",
        ] {
            assert!(
                !style.contains(scoped),
                "{scoped} scopes the base rule to a container, and every time that has been \
                 done a button outside it has shipped unstyled"
            );
        }
    }

    #[test]
    fn the_activity_card_does_not_poll_while_it_is_shut() {
        // A card nobody is looking at is a request nobody needs, twice a second, on a
        // machine whose job is to transcode video.
        let script = PAGE
            .split_once("async function metricsTick()")
            .map(|(_, rest)| rest)
            .expect("the activity card has a poll");
        let body = &script[..script.find("\n}").unwrap_or(script.len())];

        assert!(
            body.contains("folded.has(\"metrics\")"),
            "it checks whether the card is shut: {body}"
        );
        assert!(
            body.contains("document.hidden"),
            "and whether the tab is in the background: {body}"
        );
    }

    #[test]
    fn the_only_absolute_url_on_the_page_points_at_this_machine() {
        // The blanket "no http:// anywhere" rule this replaces was right about assets
        // and wrong about the one link the page has to offer: Plex's own interface, on
        // port 32400 of the appliance itself. It cannot be relative -- it is a different
        // port.
        //
        // So the rule is sharper rather than looser: every absolute URL, of either
        // scheme, must be built from the host the page was served from. A CDN or a font
        // still fails, which is what the original test existed to catch.
        //
        // The scheme is `https`, asked for so that a console served over TLS does not
        // hand out a cleartext link. It is the one claim here this repository cannot
        // check for itself: whether Plex answers TLS on 32400, and under a certificate a
        // browser accepts for a bare address, is a question about Plex. If it turns out
        // not to, the remedy is the scheme in this one anchor.
        let mut absolute = Vec::new();
        for scheme in ["http://", "https://"] {
            for (index, _) in PAGE.match_indices(scheme) {
                absolute.push(&PAGE[index + scheme.len()..]);
            }
        }
        assert_eq!(absolute.len(), 1, "exactly one absolute URL is expected");
        for after in absolute {
            assert!(
                after.starts_with("${esc(location.hostname)}"),
                "an absolute URL must be built from this machine's own address, but the \
                 page has: {}",
                &after[..after.len().min(60)]
            );
        }
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
        let (credential, _shown) = claim(&path, &mut |line| lines.push(line.to_owned()));
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
            &Services {
                provision: std::sync::Arc::clone(&job),
                ..Services::new()
            },
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
        let (second, _shown) = claim(&path, &mut |line| lines.push(line.to_owned()));

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
        let (credential, _shown) = claim(unwritable, &mut |line| lines.push(line.to_owned()));

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
