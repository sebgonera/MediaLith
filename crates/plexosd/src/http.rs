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
}

impl Request {
    /// Whether a body should be sent for this method.
    #[must_use]
    pub fn wants_body(&self) -> bool {
        self.method != "HEAD"
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

    Ok(Request {
        method: method.to_owned(),
        path: path.to_owned(),
    })
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
    handler: &(impl Fn(&Request) -> Response + ?Sized),
) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let (response, with_body) = match read_head(&mut reader)? {
        Err(error) => (error.response(), true),
        Ok(head) => match parse_request(&head) {
            Err(error) => (error.response(), true),
            Ok(request) => {
                let with_body = request.wants_body();
                (route(&request, handler), with_body)
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
pub fn route(request: &Request, handler: &(impl Fn(&Request) -> Response + ?Sized)) -> Response {
    if request.method != "GET" && request.method != "HEAD" {
        // See the module documentation: nothing here may change the machine while
        // there is no authentication in front of it.
        return Response::text(
            405,
            "plexosd serves a read-only status console; only GET and HEAD are accepted\n",
        );
    }
    handler(request)
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
pub fn serve<F>(listener: &TcpListener, handler: F, log: &mut dyn FnMut(&str)) -> io::Result<()>
where
    F: Fn(&Request) -> Response + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);

    for incoming in listener.incoming() {
        match incoming {
            Ok(mut stream) => {
                let handler = std::sync::Arc::clone(&handler);
                std::thread::spawn(move || {
                    let _ = handle(&mut stream, handler.as_ref());
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

    fn get(path: &str) -> Request {
        Request {
            method: "GET".to_owned(),
            path: path.to_owned(),
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
    fn every_route_is_read_only() {
        // The console has no authentication, which is defensible only while nothing it
        // serves can change the machine. If a verb is ever added, this fails first and
        // the fix is authentication, not deleting the test.
        let handler = |_: &Request| Response::text(200, "should not be reached");
        for method in ["POST", "PUT", "DELETE", "PATCH"] {
            let request = Request {
                method: method.to_owned(),
                path: "/".to_owned(),
            };
            let response = route(&request, &handler);
            assert_eq!(response.status, 405, "{method} must not be answered");
        }
    }

    #[test]
    fn head_returns_the_headers_of_the_get_without_the_body() {
        let handler = |_: &Request| Response::html("<p>hello</p>");
        let request = Request {
            method: "HEAD".to_owned(),
            path: "/".to_owned(),
        };
        let response = route(&request, &handler);
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
        assert_eq!(route(&get("/"), &handler).status, 200);
        assert_eq!(route(&get("/nope"), &handler).status, 404);
    }
}
