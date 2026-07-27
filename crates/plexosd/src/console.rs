//! The status console: the routes, and the page.
//!
//! This is the first thing in PlexOS a person interacts with rather than reads off a
//! kernel console, and its job is narrow: answer "is this machine working, and if not,
//! what do I do about it" from a browser on another device.
//!
//! # It shows; it does not do
//!
//! Every route here is read-only, and [`http`] refuses any method but
//! `GET` and `HEAD` so that stays true. There is no authentication, which is
//! defensible exactly as long as that holds — see the note in
//! [`http`]'s documentation. The moment a route can change the machine,
//! authentication has to come first.
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
#[must_use]
pub fn respond(request: &Request, env: &impl Environment) -> Response {
    match request.path.as_str() {
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
                "no such page: {other}\n\nThis console serves / , /api/status, /api/gpu \
                 and /healthz.\n"
            ),
        ),
    }
}

/// Brings the network up, then serves the console until the listener fails.
///
/// The network is configured first and its failure is **reported, not propagated**: a
/// machine with no cable should still serve the console to anyone who reaches it by
/// another route, and more importantly should still be running so the console on the
/// machine itself can say why. This function is called after the health gate has
/// already returned its verdict, so nothing it does can affect a rollback.
///
/// # Errors
/// Fails only if the port cannot be bound — almost always because something else holds
/// it, or because the daemon is not running as root and the port is below 1024.
pub fn run(port: u16, log: &mut dyn FnMut(&str)) -> io::Result<()> {
    match net::configure(&System, net::LINK_TIMEOUT, log) {
        Ok(interface) => log(&format!("network configured on {}", interface.name)),
        Err(error) => log(&format!("no network: {error}")),
    }

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

    for found in net::addresses(&System) {
        log(&format!("console at http://{}/", found.ip()));
    }
    log(&format!("console listening on {address}"));

    http::serve(&listener, |request| respond(request, &System), log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_gpu::env::Fixture;

    fn get(path: &str) -> Request {
        Request {
            method: "GET".to_owned(),
            path: path.to_owned(),
        }
    }

    #[test]
    fn the_root_path_serves_the_page() {
        let response = respond(&get("/"), &Fixture::new());
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
        assert_eq!(respond(&get("/api/status"), &Fixture::new()).status, 200);
    }

    #[test]
    fn the_page_carries_no_external_references() {
        // /usr is read-only and the appliance may have no route off the LAN. Anything
        // fetched from elsewhere renders this page unstyled in exactly the situation
        // it exists for: a machine whose network is broken.
        for marker in [
            "http://",
            "https://",
            "<script src",
            "<link",
            "<img",
            "@import",
        ] {
            assert!(
                !PAGE.contains(marker),
                "the page must be self-contained, but it contains {marker:?}"
            );
        }
    }

    #[test]
    fn the_status_route_returns_parsable_json() {
        let response = respond(&get("/api/status"), &Fixture::new());
        assert_eq!(response.status, 200);
        let parsed: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert!(parsed.get("gpu").is_some());
    }

    #[test]
    fn the_gpu_route_returns_the_report_alone() {
        let response = respond(&get("/api/gpu"), &Fixture::new());
        let parsed: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert!(
            parsed.get("health").is_some() && parsed.get("findings").is_some(),
            "the report itself, not the wrapper: {parsed}"
        );
    }

    #[test]
    fn an_unknown_path_lists_the_ones_that_exist() {
        // Every diagnostic names a remedy, including a 404.
        let response = respond(&get("/dashboard"), &Fixture::new());
        assert_eq!(response.status, 404);
        let body = String::from_utf8(response.body).unwrap();
        assert!(
            body.contains("/api/status"),
            "names what does exist: {body}"
        );
    }
}
