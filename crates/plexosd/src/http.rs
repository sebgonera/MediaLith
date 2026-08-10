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
//! # Where authentication sits
//!
//! In [`route`], in front of the handler, and not in the handlers themselves. Anything
//! that is not a `GET` or a `HEAD` must present the device token (ADR-0013) before a
//! handler sees it, so a route added to the console is authenticated by construction
//! rather than by its author remembering to be. `no_mutating_route_is_reachable_without_
//! the_device_token` is what keeps that true.
//!
//! The safe list is the one that needs maintaining: [`Request::is_mutating`] treats
//! every unknown verb as a write, so a method added to HTTP tomorrow arrives needing a
//! credential rather than arriving without one because nobody listed it.
//!
//! **There is still no TLS.** A token crossing an unencrypted LAN is visible to anything
//! that can see the traffic, which ADR-0013 accepts for v1 and records as the weakest
//! part of the design.

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
    /// Where to send the client instead, for a redirect.
    ///
    /// Part of the type rather than a header a caller remembers to add: a 301 without a
    /// `Location` is a status code that tells a browser to go somewhere and does not say
    /// where, which browsers render as an error page about this machine.
    pub location: Option<String>,
}

impl Response {
    /// A `200` carrying HTML.
    #[must_use]
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
            location: None,
        }
    }

    /// A `200` carrying JSON.
    #[must_use]
    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: body.into(),
            location: None,
        }
    }

    /// A plain-text response with an explicit status.
    #[must_use]
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
            location: None,
        }
    }

    /// A permanent redirect to `target`.
    ///
    /// 308 rather than 301: it preserves the method and the body, and a `POST /api/update`
    /// that arrived in clear must not be silently turned into a `GET` of the same path by
    /// a browser following the older code. It will be sent again over TLS, which is the
    /// whole point.
    #[must_use]
    pub fn redirect(target: &str) -> Self {
        Self {
            status: 308,
            content_type: "text/plain; charset=utf-8",
            body: format!("This console is served over HTTPS: {target}\n").into_bytes(),
            location: Some(target.to_owned()),
        }
    }

    /// The reason phrase, for the status line.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            308 => "Permanent Redirect",
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
             {}\r\n",
            self.status,
            self.reason(),
            self.content_type,
            self.body.len(),
            match &self.location {
                Some(target) => format!("Location: {target}\r\n"),
                None => String::new(),
            }
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

/// The largest upload this console will accept, for the one route that streams.
///
/// Plex's package is about 83 MB and grows; a quarter of a gibibyte leaves room for that
/// without the number being an invitation. It is not [`MAX_BODY`] and must never become
/// it: everything else here is a few hundred bytes of JSON, and the reason that limit is
/// 64 KiB is that a hostile `Content-Length` must not be able to name an allocation.
///
/// This one never allocates what it is told. The bytes go to a file in fixed-size chunks
/// as they arrive, so the declared length bounds *the transfer*, not memory.
pub const MAX_UPLOAD: u64 = 256 * 1024 * 1024;

/// How much is moved from socket to disk at a time.
const UPLOAD_CHUNK: usize = 64 * 1024;

/// Streams a declared body to `sink`, without ever holding it in memory.
///
/// Returns how many bytes were written. The declared `Content-Length` is checked against
/// [`MAX_UPLOAD`] before a byte is read, and the transfer is cut off at the declared
/// length even if the client keeps sending — a client that promises 80 MB and sends 400
/// must not be able to write 400 MB into `/var`.
///
/// # Errors
/// Fails on I/O in either direction. A short body — the client promising more than it
/// sends — is an error rather than a truncated file that looks like a corrupt package,
/// because the two would be indistinguishable afterwards and only one of them is Plex's
/// fault.
pub fn stream_body(
    stream: &mut (impl BufRead + ?Sized),
    request: &Request,
    sink: &mut impl Write,
) -> io::Result<Result<u64, ParseError>> {
    let Some(declared) = request.header("Content-Length") else {
        return Ok(Err(ParseError::Malformed));
    };
    let Ok(length) = declared.trim().parse::<u64>() else {
        return Ok(Err(ParseError::Malformed));
    };
    if length == 0 || length > MAX_UPLOAD {
        return Ok(Err(ParseError::TooLarge));
    }

    let mut left = length;
    let mut buffer = vec![0_u8; UPLOAD_CHUNK];
    while left > 0 {
        let want = usize::try_from(left.min(UPLOAD_CHUNK as u64)).unwrap_or(UPLOAD_CHUNK);
        stream.read_exact(&mut buffer[..want])?;
        sink.write_all(&buffer[..want])?;
        left -= want as u64;
    }
    sink.flush()?;
    Ok(Ok(length))
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
///
/// Generic over the stream because the same code serves a plain socket and a TLS one, and
/// the alternative -- a second copy of the request path behind TLS -- is two parsers that
/// drift. The timeouts are set by the caller, which is the only place that still knows it
/// is holding a `TcpStream`.
fn handle<S: Read + Write>(
    stream: &mut S,
    credential: &crate::auth::Credential,
    handler: &(impl Fn(&Request) -> Response + ?Sized),
    upload: &(impl Fn(&Request, &mut dyn BufRead) -> Option<Response> + ?Sized),
) -> io::Result<()> {
    // Scoped, so the borrow ends before the response is written. Anything the reader
    // buffered past the body is discarded with it, which is already this server's
    // behaviour: one request per connection, no keep-alive.
    let mut reader = BufReader::new(&mut *stream);
    let (response, with_body) = match read_head(&mut reader)? {
        Err(error) => (error.response(), true),
        Ok(head) => match parse_request(&head) {
            Err(error) => (error.response(), true),
            Ok(mut request) => {
                let with_body = request.wants_body();
                // Authorisation first, and for the streaming route that ordering is the
                // whole point: refusing after the body has been read would mean an
                // unauthenticated client could still make the appliance receive eighty
                // megabytes. `refusal` is the same policy `route` applies, called once.
                if let Some(refused) = refusal(&request, credential) {
                    (refused, with_body)
                } else if let Some(response) = upload(&request, &mut reader) {
                    // The route took the socket and read the body itself.
                    (response, with_body)
                } else {
                    match read_body(&mut reader, &request)? {
                        Err(error) => (error.response(), with_body),
                        Ok(body) => {
                            request.body = body;
                            (handler(&request), with_body)
                        }
                    }
                }
            }
        },
    };

    drop(reader);
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
    match refusal(request, credential) {
        Some(refused) => refused,
        None => handler(request),
    }
}

/// Why this request may not proceed, or `None` if it may.
///
/// Split out of [`route`] for the upload path, which has to decide this **before** it
/// reads a body rather than after. Reading eighty megabytes off the socket and then
/// answering 401 would make an unauthenticated client's upload cost the appliance the
/// whole transfer, which is a denial of service with a polite error at the end of it.
///
/// One function so there is one policy. Two copies of an authorisation rule is how a
/// route ends up quietly exempt.
#[must_use]
pub fn refusal(request: &Request, credential: &crate::auth::Credential) -> Option<Response> {
    if !request.is_mutating() {
        return None;
    }

    // Everything past here changes the machine, and ADR-0013 says a token comes first.
    Some(match credential {
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
                Some(token) if crate::auth::matches(token, fingerprint) => return None,
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
    })
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
pub fn serve<F, U>(
    listener: &TcpListener,
    credential: crate::auth::Credential,
    handler: F,
    upload: U,
    log: &mut dyn FnMut(&str),
) -> io::Result<()>
where
    F: Fn(&Request) -> Response + Send + Sync + 'static,
    U: Fn(&Request, &mut dyn BufRead) -> Option<Response> + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    let upload = std::sync::Arc::new(upload);

    // Installed once, at startup, and read from `auth` per connection rather than
    // captured here. The original reasoning against re-reading held that a credential
    // file replaced mid-session must not take effect without a restart, because that is
    // a way past the console rather than through it. That is still true: nothing reads
    // the file again. What this allows is the console *deliberately* swapping it, which
    // is what rotation is -- a token rotated because it leaked has to stop working now.
    crate::auth::install(credential);

    for incoming in listener.incoming() {
        match incoming {
            Ok(mut stream) => {
                let handler = std::sync::Arc::clone(&handler);
                let upload = std::sync::Arc::clone(&upload);
                std::thread::spawn(move || {
                    let credential = crate::auth::current();
                    if set_timeouts(&stream).is_ok() {
                        let _ = handle(&mut stream, &credential, handler.as_ref(), upload.as_ref());
                    }
                });
            }
            Err(error) => log(&format!("could not accept a connection: {error}")),
        }
    }
    Ok(())
}

/// Serves the console over TLS (ADR-0014).
///
/// The same handler and the same request path as [`serve`]; only the bytes on the wire
/// differ. A handshake that fails is logged and the connection dropped — the commonest
/// cause by far is somebody typing `http://` at this port, and the second is a browser
/// refusing the self-signed certificate, neither of which is a fault of this machine.
///
/// # Errors
/// If accepting fails in a way that ends the loop.
pub fn serve_tls<F, U>(
    listener: &TcpListener,
    config: &std::sync::Arc<rustls::ServerConfig>,
    credential: crate::auth::Credential,
    handler: F,
    upload: U,
    log: &mut dyn FnMut(&str),
) -> io::Result<()>
where
    F: Fn(&Request) -> Response + Send + Sync + 'static,
    U: Fn(&Request, &mut dyn BufRead) -> Option<Response> + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    let upload = std::sync::Arc::new(upload);
    crate::auth::install(credential);

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let handler = std::sync::Arc::clone(&handler);
                let upload = std::sync::Arc::clone(&upload);
                let config = std::sync::Arc::clone(config);
                std::thread::spawn(move || {
                    let credential = crate::auth::current();
                    if set_timeouts(&stream).is_err() {
                        return;
                    }
                    // A failure here is not per-connection: the configuration was
                    // accepted when the console started, or the console never started.
                    if let Ok(connection) = rustls::ServerConnection::new(config) {
                        let mut tls = rustls::StreamOwned::new(connection, stream);
                        let _ = handle(&mut tls, &credential, handler.as_ref(), upload.as_ref());
                    }
                });
            }
            Err(error) => log(&format!("could not accept a connection: {error}")),
        }
    }
    Ok(())
}

/// Answers every request with a redirect to the same path over HTTPS.
///
/// The console listens on TLS only, so this exists to keep `http://<address>/` — which is
/// what a person types and what every previous note in this repository tells them to type
/// — from being a blank refusal.
///
/// It reads the request head to recover the `Host`, and nothing else. In particular it
/// never looks at the body and never at the credential: a request that reaches this port
/// has already been sent in clear, and the only useful thing left to do with it is to say
/// where the encrypted door is.
///
/// # Errors
/// If accepting fails in a way that ends the loop.
pub fn serve_redirect(listener: &TcpListener, log: &mut dyn FnMut(&str)) -> io::Result<()> {
    for incoming in listener.incoming() {
        match incoming {
            Ok(mut stream) => {
                std::thread::spawn(move || {
                    if set_timeouts(&stream).is_err() {
                        return;
                    }
                    let mut reader = BufReader::new(match stream.try_clone() {
                        Ok(clone) => clone,
                        Err(_) => return,
                    });
                    let target = match read_head(&mut reader) {
                        Ok(Ok(head)) => redirect_target(&head),
                        _ => None,
                    };
                    let response = match target {
                        Some(target) => Response::redirect(&target),
                        None => Response::text(400, "This console speaks HTTPS. Try https://"),
                    };
                    let _ = stream.write_all(&response.to_bytes(true));
                    let _ = stream.flush();
                });
            }
            Err(error) => log(&format!("could not accept a connection: {error}")),
        }
    }
    Ok(())
}

/// Where a cleartext request should be sent instead.
///
/// `None` when the request names no host, which a browser never does and a scanner often
/// does — there is nowhere to send it, and inventing an address would be guessing at the
/// one thing the client already knew.
#[must_use]
pub fn redirect_target(head: &str) -> Option<String> {
    let request = parse_request(head).ok()?;
    let host = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.as_str())?;

    // The port is dropped rather than translated. It is this listener's port -- the
    // cleartext one -- and carrying it across would send the browser to https on the http
    // port, which is a connection that hangs rather than one that fails.
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return None;
    }

    // The whole request target, query string included. `Request::path` drops the query on
    // purpose, because no route here takes one -- but this is not a route. It is the same
    // address over TLS, and a redirect that quietly rewrites what was asked for sends
    // somebody somewhere they did not ask to go.
    //
    // Only an origin-form target is pasted through. An absolute-form one -- `GET
    // http://host/path`, which is what a proxy sends -- would otherwise be concatenated
    // onto the host and produce `https://192.168.2.102http://...`, so it becomes the
    // console root, which is somewhere that exists.
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .filter(|target| target.starts_with('/'))
        .unwrap_or("/");
    Some(format!("https://{host}{target}"))
}

/// Read and write deadlines, so one stuck client cannot hold a thread for ever.
fn set_timeouts(stream: &TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))
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
    fn a_streamed_body_reaches_the_sink_whole_and_is_never_a_buffer() {
        // The reason MAX_BODY exists is that a declared length must not be an
        // allocation. This route accepts a length two thousand times larger, so it has
        // to hold that property a different way: bytes move socket to sink in fixed
        // chunks. The size here spans several chunks, since a body that fits in one
        // would exercise none of the loop.
        let payload: Vec<u8> = (0..200_000_u32).map(|n| (n % 251) as u8).collect();
        let head = format!("Content-Length: {}", payload.len());
        let request = Request {
            method: "POST".to_owned(),
            path: UPLOAD_PATH_FOR_TEST.to_owned(),
            headers: vec![(
                "Content-Length".to_owned(),
                head["Content-Length: ".len()..].to_owned(),
            )],
            body: Vec::new(),
        };

        let mut stream = std::io::BufReader::new(payload.as_slice());
        let mut sink: Vec<u8> = Vec::new();
        let written = stream_body(&mut stream, &request, &mut sink)
            .unwrap()
            .unwrap();

        assert_eq!(written, payload.len() as u64);
        assert_eq!(sink, payload, "the sink must hold exactly what was sent");
    }

    #[test]
    fn a_client_that_keeps_sending_is_cut_off_at_what_it_declared() {
        // Otherwise a client could declare eighty megabytes and write until /var is
        // full, and the declared length would bound nothing at all.
        let request = Request {
            method: "POST".to_owned(),
            path: UPLOAD_PATH_FOR_TEST.to_owned(),
            headers: vec![("Content-Length".to_owned(), "10".to_owned())],
            body: Vec::new(),
        };
        let sent = b"0123456789and a great deal more than was promised";
        let mut stream = std::io::BufReader::new(&sent[..]);
        let mut sink: Vec<u8> = Vec::new();

        assert_eq!(
            stream_body(&mut stream, &request, &mut sink).unwrap(),
            Ok(10)
        );
        assert_eq!(sink, b"0123456789", "only the declared bytes are written");
    }

    #[test]
    fn a_short_body_is_an_error_rather_than_a_truncated_package() {
        // A client that promises more than it sends. Left as a short file it would be
        // verified later and reported as a bad signature, which blames Plex for a
        // transfer this machine dropped.
        let request = Request {
            method: "POST".to_owned(),
            path: UPLOAD_PATH_FOR_TEST.to_owned(),
            headers: vec![("Content-Length".to_owned(), "5000".to_owned())],
            body: Vec::new(),
        };
        let mut stream = std::io::BufReader::new(&b"far too little"[..]);
        let mut sink: Vec<u8> = Vec::new();
        assert!(stream_body(&mut stream, &request, &mut sink).is_err());
    }

    #[test]
    fn an_upload_beyond_the_limit_is_refused_before_a_byte_is_read() {
        let request = Request {
            method: "POST".to_owned(),
            path: UPLOAD_PATH_FOR_TEST.to_owned(),
            headers: vec![("Content-Length".to_owned(), (MAX_UPLOAD + 1).to_string())],
            body: Vec::new(),
        };
        let mut stream = std::io::BufReader::new(&b""[..]);
        let mut sink: Vec<u8> = Vec::new();
        assert_eq!(
            stream_body(&mut stream, &request, &mut sink).unwrap(),
            Err(ParseError::TooLarge)
        );
        assert!(
            sink.is_empty(),
            "nothing may be read before the length is judged"
        );
    }

    #[test]
    fn the_bounded_limit_stays_where_it_is() {
        // MAX_UPLOAD exists so that MAX_BODY does not have to move. If somebody ever
        // raises the bounded one to make an upload fit, every other route on this
        // console gains the ability to name a 256 MiB allocation from a header.
        assert_eq!(MAX_BODY, 64 * 1024);
        assert!(
            MAX_UPLOAD > MAX_BODY as u64,
            "the streaming route is the one that takes something large"
        );
    }

    /// The upload path, spelled here rather than imported, so that the two files have to
    /// agree explicitly. `console` owns the route; this owns the transport.
    const UPLOAD_PATH_FOR_TEST: &str = "/api/provision/upload";

    #[test]
    fn the_transport_and_the_route_agree_about_the_path() {
        assert_eq!(UPLOAD_PATH_FOR_TEST, crate::provision::UPLOAD_PATH);
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

    #[test]
    fn a_cleartext_request_is_told_where_the_encrypted_door_is() {
        // What a person gets for typing the address every note in this repository gave
        // them. Without it the console is a refused connection on a machine that is
        // working perfectly.
        let head = "GET /api/status HTTP/1.1\r\nHost: 192.168.2.102\r\n";
        assert_eq!(
            redirect_target(head).as_deref(),
            Some("https://192.168.2.102/api/status")
        );
    }

    #[test]
    fn the_cleartext_port_is_not_carried_across() {
        // It is *this* listener's port. Carrying it over sends the browser to https on
        // the http port, which hangs rather than failing -- the worse of the two.
        let head = "GET / HTTP/1.1\r\nHost: 192.168.2.102:80\r\n";
        assert_eq!(
            redirect_target(head).as_deref(),
            Some("https://192.168.2.102/")
        );
    }

    #[test]
    fn a_redirect_keeps_the_query_string() {
        // The parser drops the query on purpose -- no route here reads one -- and the
        // redirect was built from the parsed path, so it quietly sent the browser to a
        // different address from the one it asked for. Nothing on this console uses a
        // query today, which is exactly why this would have been found by somebody
        // bookmarking a link, long after anybody remembered writing it.
        let head = "GET /api/status?x=1&y=2 HTTP/1.1\r\nHost: 192.168.2.102\r\n";
        assert_eq!(
            redirect_target(head).as_deref(),
            Some("https://192.168.2.102/api/status?x=1&y=2")
        );
    }

    #[test]
    fn an_absolute_target_is_not_pasted_into_the_redirect() {
        // Proxies send `GET http://host/path`. Pasting that after `https://<host>` would
        // produce `https://192.168.2.102http://elsewhere.example/path`, which is what the
        // first version of this did. The console root is somewhere that exists.
        let head = "GET http://elsewhere.example/path HTTP/1.1\r\nHost: 192.168.2.102\r\n";
        assert_eq!(
            redirect_target(head).as_deref(),
            Some("https://192.168.2.102/")
        );
    }

    #[test]
    fn a_request_naming_no_host_is_not_sent_anywhere_invented() {
        // Browsers always send Host; scanners often do not. There is nowhere to send it,
        // and guessing an address would be guessing at the one thing the client knew.
        assert_eq!(redirect_target("GET / HTTP/1.1\r\n"), None);
        assert_eq!(redirect_target("GET / HTTP/1.1\r\nHost: \r\n"), None);
    }

    #[test]
    fn a_redirect_carries_the_location_it_names() {
        // A 308 without a Location is a status code that tells a browser to go somewhere
        // and does not say where, which renders as an error page about this machine.
        let bytes = Response::redirect("https://192.168.2.102/").to_bytes(true);
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.starts_with("HTTP/1.1 308 Permanent Redirect\r\n"),
            "{text}"
        );
        assert!(
            text.contains("Location: https://192.168.2.102/\r\n"),
            "{text}"
        );
    }

    #[test]
    fn the_console_speaks_tls_to_a_client_that_is_not_this_code() {
        // The check the console page taught this project to write. `cargo build` proves a
        // TLS stack compiles; it does not prove a browser can talk to it, and the cost of
        // finding that out on the appliance is an image that boots to no console at all.
        //
        // curl rather than a rustls client: an independent implementation, the same
        // reason the verity digest is pinned against sha256sum rather than against
        // another call to the same crate. `-k` because the certificate is self-signed by
        // design -- what is under test is the handshake and the request path, not trust.
        let curl = std::process::Command::new("curl").arg("--version").output();
        if !curl.is_ok_and(|out| out.status.success()) {
            println!("skip: no curl on this host, so the handshake was not exercised");
            return;
        }

        let dir = std::env::temp_dir().join("plexos-tls-handshake");
        let _ = std::fs::remove_dir_all(&dir);
        let identity =
            crate::tls::load_or_create(&dir, &["localhost".to_owned()]).expect("an identity");
        let config = crate::tls::server_config(&identity).expect("a server config");

        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().unwrap().port();

        std::thread::spawn(move || {
            let mut log = |_: &str| {};
            let _ = serve_tls(
                &listener,
                &config,
                crate::auth::Credential::Unset,
                |request| Response::text(200, format!("served {}", request.path)),
                |_: &Request, _: &mut dyn BufRead| None,
                &mut log,
            );
        });

        let out = std::process::Command::new("curl")
            .args(["--silent", "--show-error", "--insecure", "--max-time", "20"])
            .arg(format!("https://localhost:{port}/hello"))
            .output()
            .expect("curl runs");

        let body = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "curl could not complete a handshake: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(body.trim(), "served /hello");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
