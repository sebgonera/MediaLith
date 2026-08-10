//! Serves the real route table with a known token, so the gate can be tried over HTTP.
//!
//! ```text
//! cargo run -p plexosd --example authgate -- <port> <token>
//! ```
//!
//! For checking by hand what the unit tests check in memory: that a mutating request
//! without a token is refused, that a wrong token is refused differently, and that
//! reading needs nothing at all. Not shipped on the appliance.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};

use plexosd::http::{self, Request, Response};

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8099);
    let token = args.next().unwrap_or_else(|| "x".repeat(64));

    let credential = plexosd::auth::Credential::Set(plexosd::auth::fingerprint(&token));
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    eprintln!("listening on {port}");

    http::serve(
        &listener,
        credential,
        |request: &Request| match request.path.as_str() {
            "/api/status" => Response::json(br#"{"ok":true}"#.to_vec()),
            "/api/provision" => Response::text(200, "would provision\n"),
            other => Response::text(404, format!("no such route: {other}\n")),
        },
        &mut |line| eprintln!("{line}"),
    )
}
