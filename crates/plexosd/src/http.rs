//! A small HTTP/1.1 server, hand-written.
//!
//! # Why not a web framework
//!
//! The same reason `plexos-sys` wraps its own syscalls instead of taking `rustix`:
//! this binary ships inside a verity-sealed, read-only `/usr` on an appliance that
//! must keep working with nothing to go stale. What it serves is a handful of
//! read-only routes over a JSON document. A framework and its dependency tree would be
//! several orders of magnitude more code than the thing being served, all of it
//! arriving on the appliance's attack surface and in every future audit.
//!
//! The cost is real and worth stating: this speaks a deliberately small subset of
//! HTTP/1.1. No keep-alive, no chunked encoding, no compression, no TLS, `GET` and
//! `HEAD` only. Every response carries `Content-Length` and closes the connection.
//! Browsers handle that correctly; it is simply slower than it could be, which for a
//! page one person loads occasionally is not a cost worth paying code for.
//!
//! # What this deliberately does not do
//!
//! **There is no authentication.** Every route is read-only — nothing here changes the
//! machine — and the appliance is expected to sit on a home LAN. That reasoning stops
//! being sufficient the moment a route can *do* something, so
//! `every_route_is_read_only` fails if a non-`GET` method is ever answered with
//! anything but 405. When the management API grows verbs, it needs authentication
//! before it grows them, and that test is the reminder.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Largest request head accepted, in bytes.
///
/// A client that never sends a blank line would otherwise grow this buffer until the
/// appliance runs out of memory. Real browser requests are a few hundred bytes.
pub const MAX_HEAD: usize = 8 * 1024;

/// Largest request body accepted.
///
/// 64 KiB, which is generous for the JSON these routes take and small enough that a
/// hostile `Content-Length` cannot be a memory limit. Uploading an app image will not
/// come through here: 83 MB has to be streamed to disk, and a route that does that
/// reads the socket itself rather than being handed a `Vec`.
pub const MAX_BODY: usize = 64 * 1024;

/// How long a client may take to send its request, or to accept the response.
///
/// Without this a single half-open connection holds a thread forever.
pub const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// A parsed request. Only what the routes actually use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `GET`, `HEAD`, or whatever the client sent.
    pub method: String,
    /// Path with the query string removed, percent-encoding untouched.
    pub path: String,
    /// Header fields, in the order sent.
    pub headers: Vec<(String, String)>,
    /// The request body, if one was sent and small enough to hold.
    ///
    /// Bounded by [`MAX_BODY`]. This console's mutating routes take a few hundred
    /// bytes of parameters; anything claiming megabytes is refused rather than
    /// buffered, because a buffer sized by a header is a memory limit set by whoever
    /// connected.
    pub body: Vec<u8>,
}

impl Request {
    /// Whether a body should be sent for this method.
    #[must_use]
    pub fn wants_body(&self) -> bool {
        self.method != "HEAD"
    }

    /// One header, matched without regard to case.
    ///
    /// HTTP field names are case-insensitive and clients differ: `curl` sends
    /// `Authorization`, some libraries send `authorization`. Comparing exactly means a
    /// token that works from one client is rejected from another, with a 401 that
    /// blames the credential rather than the comparison.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether this request could change the machine.
    ///
    /// Anything that is not a read counts, including methods nobody has implemented.
    /// The list that needs maintaining is therefore the *safe* one: a verb added to
    /// HTTP tomorrow arrives needing a token, rather than arriving unauthenticated
    /// because no one thought to list it.
    #[must_use]
    pub fn is_mutating(&self) -> bool {
        !matches!(self.method.as_str(), "GET" | "HEAD")
    }
}

/// A response, complete before any of it is written.
///
/// Built whole rather than streamed so that `Content-Length` is always correct: a
/// truncated response with a plausible length is far harder to diagnose than a
/// connection that failed outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Value for the `Content-Type` header.
    pub content_type: &'static str,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// A `200` carrying HTML.
    #[must_use]
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
        }
    }

    /// A `200` carrying JSON.
    #[must_use]
    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: body.into(),
        }
    }

    /// A plain-text response with an explicit status.
    #[must_use]
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
        }
    }

    /// The reason phrase, for the status line.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Content Too Large",
            500 => "Internal Server Error",
            _ => "Unknown",
        }
    }

    /// Serialises the response, including the body unless the request forbids one.
    ///
    /// `Content-Length` is emitted even when the body is omitted, because that is what
    /// `HEAD` means: the headers a `GET` would have produced.
    #[must_use]
    pub fn to_bytes(&self, with_body: bool) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Cache-Control: no-store\r\n\
             Connection: close\r\n\
             \r\n",
            self.status,
            self.reason(),
            self.content_type,
            self.body.len()
        )
        .into_bytes();

        if with_body {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

/// Why a request could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The request line was absent or had too few fields.
    Malformed,
    /// The head exceeded [`MAX_HEAD`].
    TooLarge,
}

impl ParseError {
    /// The response this error deserves.
    #[must_use]
    pub fn response(self) -> Response {
        match self {
            Self::Malformed => Response::text(400, "malformed request\n"),
            Self::TooLarge => Response::text(413, "request head too large\n"),
        }
    }
}

/// Parses a request head — everything up to the blank line.
///
/// Headers are read and discarded: no route depends on one. That is worth being
/// explicit about rather than silently not parsing them.
///
/// # Errors
/// Returns [`ParseError::Malformed`] if the request line is missing or has fewer than
/// two space-separated fields.
pub fn parse_request(head: &str) -> Result<Request, ParseError> {
    let line = head.lines().next().ok_or(ParseError::Malformed)?;
    let mut fields = line.split(' ');

    let method = fields
        .next()
        .filter(|m| !m.is_empty())
        .ok_or(ParseError::Malformed)?;
    let target = fields
        .next()
        .filter(|t| !t.is_empty())
        .ok_or(ParseError::Malformed)?;

    // The query string is split off and dropped. If a route ever needs one, this is
    // where it stops being dropped -- but a route that takes parameters is a route
    // that does something, and see the note about authentication in the module docs.
    let path = target.split('?').next().unwrap_or(target);

    // Header lines are everything after the request line. A line without a colon is
    // dropped rather than failing the request: a proxy that inserts something odd
    // should not take the console down.
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect();

    Ok(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body: Vec::new(),
    })
}

/// Reads a bounded request body, given the head's `Content-Length`.
///
/// Absent or zero means no body. A length beyond [`MAX_BODY`] is refused before a byte
/// is read, so the allocation is never the one the client asked for.
fn read_body(
    stream: &mut impl BufRead,
    request: &Request,
) -> io::Result<Result<Vec<u8>, ParseError>> {
    let Some(declared) = request.header("Content-Length") else {
        return Ok(Ok(Vec::new()));
    };
    let Ok(length) = declared.trim().parse::<usize>() else {
        return Ok(Err(ParseError::Malformed));
    };
    if length == 0 {
        return Ok(Ok(Vec::new()));
    }
    if length > MAX_BODY {
        return Ok(Err(ParseError::TooLarge));
    }

    let mut body = vec![0_u8; length];
    // read_exact, so a client that promises more than it sends is an error rather than
    // a handler receiving a half-filled buffer of zeroes it cannot tell from data.
    stream.read_exact(&mut body)?;
    Ok(Ok(body))
}

/// Reads the request head from a stream, stopping at the blank line.
///
/// # Errors
/// Fails on I/O error. A head larger than [`MAX_HEAD`] is reported as
/// [`ParseError::TooLarge`] rather than read to completion.
fn read_head(stream: &mut impl BufRead) -> io::Result<Result<String, ParseError>> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let read = stream
            .take((MAX_HEAD - head.len()) as u64)
            .read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
        if head.len() >= MAX_HEAD {
            return Ok(Err(ParseError::TooLarge));
        }
    }
    Ok(Ok(head))
}

/// Handles one connection, from parse to response.
fn handle(
    stream: &mut TcpStream,
    credential: &crate::auth::Credential,
    handler: &(impl Fn(&Request) -> Response + ?Sized),
) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let (response, with_body) = match read_head(&mut reader)? {
        Err(error) => (error.response(), true),
        Ok(head) => match parse_request(&head) {
            Err(error) => (error.response(), true),
            Ok(mut request) => {
                let with_body = request.wants_body();
                match read_body(&mut reader, &request)? {
                    Err(error) => (error.response(), with_body),
                    Ok(body) => {
                        request.body = body;
                        (route(&request, credential, handler), with_body)
                    }
                }
            }
        },
    };

    stream.write_all(&response.to_bytes(with_body))?;
    stream.flush()
}

/// Applies the method policy, then defers to the handler.
///
/// Kept separate from the connection handling so the policy can be tested without a
/// socket.
#[must_use]
pub fn route(
    request: &Request,
    credential: &crate::auth::Credential,
    handler: &(impl Fn(&Request) -> Response + ?Sized),
) -> Response {
    if !request.is_mutating() {
        return handler(request);
    }

    // Everything past here changes the machine, and ADR-0013 says a token comes first.
    match credential {
        // An unclaimed device. Refusing rather than allowing is the only safe reading:
        // "no credential is set" must never mean "no credential is needed", which is
        // how appliances ship with an open management interface.
        crate::auth::Credential::Unset => Response::text(
            503,
            "This device has not been claimed yet, so there is no credential to check \
             and nothing may change it. The token is printed on the console attached to \
             the machine at first start (ADR-0013).\n",
        ),
        crate::auth::Credential::Set(fingerprint) => {
            let presented = request
                .header("Authorization")
                .and_then(crate::auth::bearer);
            match presented {
                Some(token) if crate::auth::matches(token, fingerprint) => handler(request),
                Some(_) => Response::text(
                    403,
                    "That token is not this device's. The one printed on its console at \
                     first start is the only one it accepts; deleting the credential \
                     file and restarting issues a new one.\n",
                ),
                None => Response::text(
                    401,
                    "This route changes the machine and needs the device token: send it \
                     as `Authorization: Bearer <token>`. Reading the status page needs \
                     nothing.\n",
                ),
            }
        }
    }
}

/// Serves connections until the listener fails, one thread per connection.
///
/// A thread rather than a loop because a client that stops reading mid-response would
/// otherwise stall every other client until [`IO_TIMEOUT`] expired. Threads are
/// created per connection and not pooled: this serves a status page to one person, and
/// a pool would be machinery in place of a measurement.
///
/// # Errors
/// Fails only if accepting stops working. A failure on an individual connection is
/// logged and the loop continues, because one broken client must not take the console
/// down.
pub fn serve<F>(
    listener: &TcpListener,
    credential: crate::auth::Credential,
    handler: F,
    log: &mut dyn FnMut(&str),
) -> io::Result<()>
where
    F: Fn(&Request) -> Response + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    // Read once, at startup, and shared. Re-reading per request would let a file
    // replaced mid-session take effect without a restart, which is a way to change the
    // credential that does not go through the console.
    let credential = std::sync::Arc::new(credential);

    for incoming in listener.incoming() {
        match incoming {
            Ok(mut stream) => {
                let handler = std::sync::Arc::clone(&handler);
                let credential = std::sync::Arc::clone(&credential);
                std::thread::spawn(move || {
                    let _ = handle(&mut stream, credential.as_ref(), handler.as_ref());
                });
            }
            Err(error) => log(&format!("could not accept a connection: {error}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token shaped like a real one. Not a secret: it is in a public repository.
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    /// A different one, for the wrong-token path.
    const WRONG: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn get(path: &str) -> Request {
        Request {
            method: "GET".to_owned(),
            path: path.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn a_normal_browser_request_is_parsed() {
        let head = "GET /api/status HTTP/1.1\r\nHost: 192.168.2.42\r\nAccept: */*\r\n";
        let request = parse_request(head).unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/api/status");
    }

    #[test]
    fn the_query_string_is_split_off_the_path() {
        // Browsers append cache-busting parameters; a route table that matched on the
        // raw target would 404 on the same page it had just served.
        assert_eq!(parse_request("GET /?t=17 HTTP/1.1\r\n").unwrap().path, "/");
    }

    #[test]
    fn a_request_line_without_a_target_is_rejected_not_guessed() {
        assert_eq!(parse_request("GET\r\n"), Err(ParseError::Malformed));
        assert_eq!(parse_request(""), Err(ParseError::Malformed));
    }

    #[test]
    fn no_mutating_route_is_reachable_without_the_device_token() {
        // This replaces every_route_is_read_only, which said a verb must never be
        // answered at all. The property it protected -- nothing changes this machine
        // without authority -- is unchanged; only its shape is, now that ADR-0013
        // provides the authority. Deleting it and not replacing it was never an option.
        let handler = |_: &Request| Response::text(200, "should not be reached");
        let claimed = crate::auth::Credential::Set(crate::auth::fingerprint(TOKEN));

        for method in ["POST", "PUT", "DELETE", "PATCH", "BREW"] {
            let anonymous = Request {
                method: method.to_owned(),
                path: "/api/provision".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            };
            assert_eq!(
                route(&anonymous, &claimed, &handler).status,
                401,
                "{method} without a token"
            );

            let wrong = Request {
                headers: vec![("Authorization".to_owned(), format!("Bearer {WRONG}"))],
                ..anonymous.clone()
            };
            assert_eq!(
                route(&wrong, &claimed, &handler).status,
                403,
                "{method} with the wrong token"
            );
        }
    }

    #[test]
    fn an_unclaimed_device_refuses_changes_rather_than_allowing_them() {
        // The reading that ships appliances with open management interfaces: "no
        // credential is set" taken to mean "no credential is needed". A device nobody
        // has claimed is the one most likely to be reachable and least likely to be
        // watched.
        let handler = |_: &Request| Response::text(200, "should not be reached");
        let request = Request {
            method: "POST".to_owned(),
            path: "/api/provision".to_owned(),
            headers: vec![("Authorization".to_owned(), format!("Bearer {TOKEN}"))],
            body: Vec::new(),
        };
        let response = route(&request, &crate::auth::Credential::Unset, &handler);
        assert_eq!(response.status, 503);
        assert!(
            String::from_utf8_lossy(&response.body).contains("not been claimed"),
            "and says why"
        );
    }

    #[test]
    fn the_right_token_gets_through() {
        let handler = |_: &Request| Response::text(200, "reached");
        let claimed = crate::auth::Credential::Set(crate::auth::fingerprint(TOKEN));
        let request = Request {
            method: "POST".to_owned(),
            path: "/api/provision".to_owned(),
            headers: vec![("Authorization".to_owned(), format!("Bearer {TOKEN}"))],
            body: Vec::new(),
        };
        assert_eq!(route(&request, &claimed, &handler).status, 200);
    }

    #[test]
    fn reading_still_needs_nothing_at_all() {
        // The console exists to be readable when the machine is broken. Requiring a
        // token before it will say why a boot failed defeats the reason it was built,
        // and ADR-0013 keeps reads open for exactly that.
        let handler = |_: &Request| Response::text(200, "the status page");
        for credential in [
            crate::auth::Credential::Unset,
            crate::auth::Credential::Set(crate::auth::fingerprint(TOKEN)),
        ] {
            for method in ["GET", "HEAD"] {
                let request = Request {
                    method: method.to_owned(),
                    path: "/api/status".to_owned(),
                    headers: Vec::new(),
                    body: Vec::new(),
                };
                assert_eq!(route(&request, &credential, &handler).status, 200);
            }
        }
    }

    #[test]
    fn an_unknown_verb_is_treated_as_a_write() {
        // The safe list is the one that needs maintaining. A verb added to HTTP
        // tomorrow arrives needing a token rather than arriving unauthenticated
        // because nobody listed it.
        let request = Request {
            method: "QUERY".to_owned(),
            path: "/".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert!(request.is_mutating());
    }

    #[test]
    fn a_body_within_the_bound_reaches_the_handler() {
        let head = "POST /api/provision HTTP/1.1\r\nContent-Length: 5\r\n\r\n";
        let request = parse_request(head).unwrap();
        let mut stream = BufReader::new(&b"hello"[..]);
        let body = read_body(&mut stream, &request).unwrap().unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn a_body_larger_than_the_bound_is_refused_before_it_is_allocated() {
        // The allocation must never be the one the client asked for: a Content-Length
        // of a gigabyte would otherwise be a memory limit set by whoever connected.
        let head = format!(
            "POST /x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let request = parse_request(&head).unwrap();
        let mut stream = BufReader::new(&b""[..]);
        assert_eq!(
            read_body(&mut stream, &request).unwrap(),
            Err(ParseError::TooLarge)
        );
    }

    #[test]
    fn a_short_body_is_an_error_rather_than_zeroes() {
        // A client promising ten bytes and sending three. Handing the handler a
        // half-filled buffer would give it padding it cannot tell from data.
        let head = "POST /x HTTP/1.1\r\nContent-Length: 10\r\n\r\n";
        let request = parse_request(head).unwrap();
        let mut stream = BufReader::new(&b"abc"[..]);
        assert!(read_body(&mut stream, &request).is_err());
    }

    #[test]
    fn no_content_length_means_no_body_rather_than_a_hang() {
        let request = parse_request("GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut stream = BufReader::new(&b""[..]);
        assert_eq!(read_body(&mut stream, &request).unwrap(), Ok(Vec::new()));
    }

    #[test]
    fn header_lookup_ignores_case_because_clients_differ() {
        // curl sends Authorization, some libraries send authorization. An exact
        // comparison rejects a valid token and blames the credential.
        let request = Request {
            method: "POST".to_owned(),
            path: "/".to_owned(),
            headers: vec![("authorization".to_owned(), "Bearer x".to_owned())],
            body: Vec::new(),
        };
        assert_eq!(request.header("Authorization"), Some("Bearer x"));
        assert_eq!(request.header("AUTHORIZATION"), Some("Bearer x"));
    }

    #[test]
    fn head_returns_the_headers_of_the_get_without_the_body() {
        let handler = |_: &Request| Response::html("<p>hello</p>");
        let request = Request {
            method: "HEAD".to_owned(),
            path: "/".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let response = route(&request, &crate::auth::Credential::Unset, &handler);
        assert_eq!(response.status, 200);

        let bytes = response.to_bytes(request.wants_body());
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.contains("Content-Length: 12"),
            "the length a GET would have reported: {text}"
        );
        assert!(!text.contains("hello"), "no body on HEAD: {text}");
    }

    #[test]
    fn content_length_counts_bytes_not_characters() {
        // A status detail containing a non-ASCII character would truncate in the
        // browser if this counted chars, and the page would render half a document
        // with no error anywhere.
        let response = Response::json("{\"detail\":\"zażółć\"}");
        let text = String::from_utf8(response.to_bytes(true)).unwrap();
        let body_len = "{\"detail\":\"zażółć\"}".len();
        assert!(text.contains(&format!("Content-Length: {body_len}")));
    }

    #[test]
    fn an_oversized_head_is_refused_rather_than_buffered() {
        let flood = format!("GET / HTTP/1.1\r\n{}\r\n", "X-Pad: pad\r\n".repeat(2000));
        let mut reader = BufReader::new(flood.as_bytes());
        let outcome = read_head(&mut reader).unwrap();
        assert_eq!(outcome, Err(ParseError::TooLarge));
    }

    #[test]
    fn the_head_ends_at_the_blank_line() {
        let raw = "GET / HTTP/1.1\r\nHost: x\r\n\r\nbody-that-must-not-be-read";
        let mut reader = BufReader::new(raw.as_bytes());
        let head = read_head(&mut reader).unwrap().unwrap();
        assert!(head.contains("Host: x"));
        assert!(!head.contains("body-that-must-not-be-read"));
    }

    #[test]
    fn responses_close_the_connection_since_keep_alive_is_not_implemented() {
        // Claiming HTTP/1.1 without saying this would leave a browser waiting for a
        // second response on a socket that is never going to carry one.
        let text = String::from_utf8(Response::html("x").to_bytes(true)).unwrap();
        assert!(text.contains("Connection: close"), "{text}");
    }

    #[test]
    fn the_handler_decides_what_an_unknown_path_is() {
        let handler = |r: &Request| {
            if r.path == "/" {
                Response::html("root")
            } else {
                Response::text(404, "no such page\n")
            }
        };
        assert_eq!(
            route(&get("/"), &crate::auth::Credential::Unset, &handler).status,
            200
        );
        assert_eq!(
            route(&get("/nope"), &crate::auth::Credential::Unset, &handler).status,
            404
        );
    }
}
