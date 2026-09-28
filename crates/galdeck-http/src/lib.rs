//! Just enough HTTP/1.1 to serve one local page.
//!
//! Taking a web framework would mean an async runtime, and the daemon is
//! deliberately blocking threads throughout — the device handle is `!Sync`,
//! has no file descriptor to poll, and blocks for over a second on open, so
//! there is nothing for a reactor to do. What is left to write is a request
//! parser, a response writer, and server-sent events, which is a few hundred
//! lines with no dependencies.
//!
//! Parsing and routing are pure functions over bytes so they can be tested
//! without a socket, which is where most of the tests are.

pub mod sse;

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};

/// Longest request line accepted.
///
/// Everything here is bounded, because this parser reads from a socket and an
/// unbounded read is a way to be talked out of all your memory.
pub const MAX_REQUEST_LINE: usize = 8 * 1024;
/// Largest header block accepted.
pub const MAX_HEADERS: usize = 16 * 1024;
/// Largest body accepted. Config files are the biggest thing posted here.
pub const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// Path with the query string removed.
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// Header names are lowercased; HTTP header names are case-insensitive and
    /// forgetting that is a reliable source of security holes.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn body_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Not enough bytes yet; read more and try again.
    Incomplete,
    TooLarge(&'static str),
    Malformed(&'static str),
    /// A body larger than the caller allows.
    BodyTooLarge,
}

/// How much of the head has arrived, if it has.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 4)
}

/// Parse a request from a buffer.
///
/// Returns `Incomplete` when more bytes are needed, so the caller can loop on
/// a socket read without this function knowing what a socket is.
pub fn parse_request(buffer: &[u8]) -> Result<Request, ParseError> {
    let Some(head_end) = find_head_end(buffer) else {
        if buffer.len() > MAX_REQUEST_LINE + MAX_HEADERS {
            return Err(ParseError::TooLarge("headers"));
        }
        return Err(ParseError::Incomplete);
    };
    let head = std::str::from_utf8(&buffer[..head_end - 4])
        .map_err(|_| ParseError::Malformed("headers are not utf-8"))?;

    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or(ParseError::Malformed("no request line"))?;
    if request_line.len() > MAX_REQUEST_LINE {
        return Err(ParseError::TooLarge("request line"));
    }

    let mut parts = request_line.split(' ');
    let method = parts
        .next()
        .filter(|m| !m.is_empty())
        .ok_or(ParseError::Malformed("no method"))?
        .to_string();
    let target = parts.next().ok_or(ParseError::Malformed("no target"))?;
    let version = parts.next().unwrap_or("HTTP/1.1");
    if !version.starts_with("HTTP/1.") {
        return Err(ParseError::Malformed("unsupported http version"));
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, parse_query(query)),
        None => (target, BTreeMap::new()),
    };

    let mut headers = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ParseError::Malformed("header without a colon"));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    let length: usize = match headers.get("content-length") {
        Some(value) => value
            .parse()
            .map_err(|_| ParseError::Malformed("bad content-length"))?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err(ParseError::BodyTooLarge);
    }
    if buffer.len() < head_end + length {
        return Err(ParseError::Incomplete);
    }

    Ok(Request {
        method,
        path: percent_decode(path),
        query,
        headers,
        body: buffer[head_end..head_end + length].to_vec(),
    })
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(part), String::new()),
        })
        .collect()
}

/// Decode `%xx` escapes and `+`. Invalid escapes are left as written rather
/// than dropped, so a path never silently becomes a different path.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a page served from here may load and run, sent with every response.
///
/// The UI is one module script, one stylesheet and same-origin API calls, so
/// everything else is refused: a string that slips past escaping into the
/// page's markup cannot run as script, pull in a stylesheet, or send what it
/// finds anywhere but back to this daemon. The script sets styles through the
/// CSSOM, which `style-src 'self'` allows, never through `style` attributes,
/// which it drops. `data:` images are the widget previews the gallery draws,
/// and the empty favicon that stops the browser asking for one.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'none'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            body,
        }
    }

    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::new(status, "application/json; charset=utf-8", body.into())
    }

    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/html; charset=utf-8", body.into())
    }

    pub fn text(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::new(status, "text/plain; charset=utf-8", body.into())
    }

    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Serialize head and body for writing to a socket.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 512);
        out.extend_from_slice(
            format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status)).as_bytes(),
        );
        for (name, value) in &self.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("content-length: {}\r\n", self.body.len()).as_bytes());
        // This server exists to serve one local page to one browser; there is
        // nothing here worth a cache and a stale config editor is confusing.
        out.extend_from_slice(b"cache-control: no-store\r\n");
        // Belt and braces around the Origin check: nothing here should ever be
        // framed or sniffed.
        out.extend_from_slice(b"x-content-type-options: nosniff\r\n");
        out.extend_from_slice(b"x-frame-options: DENY\r\n");
        // On every response rather than only the page's: a browser pointed at
        // an error renders that as a document too.
        out.extend_from_slice(
            format!("content-security-policy: {CONTENT_SECURITY_POLICY}\r\n").as_bytes(),
        );
        // The page asks for this in a meta tag, which an error document or a
        // preview opened on its own does not have. The token rides in the
        // query of preview and event URLs, and a Referer would carry it on.
        out.extend_from_slice(b"referrer-policy: no-referrer\r\n");
        out.extend_from_slice(b"connection: close\r\n\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        410 => "Gone",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Whether a request may act on the daemon.
///
/// This is the whole security model of the HTTP surface, so it is one function
/// and it is tested by name. The API can set shell commands the daemon runs,
/// which makes an unauthenticated port on this machine a remote code execution
/// hole reachable from any web page the user happens to visit.
///
/// It is the two halves below, in order: where the request came from, then
/// what it carries. They are separate because the sign-in endpoint needs the
/// first before there is a token to check.
pub fn authorize(request: &Request, token: &str, port: u16) -> Result<(), Response> {
    check_host_origin(request, port)?;
    check_token(request, token)
}

/// Whether a request was addressed to us, by a page of ours or by no page.
pub fn check_host_origin(request: &Request, port: u16) -> Result<(), Response> {
    // A browser will happily resolve any hostname to 127.0.0.1 and then send
    // the request with that hostname in Host. Only the literal loopback names
    // are accepted, so a DNS rebind lands on a rejection.
    let host = request.header("host").unwrap_or_default();
    let host_ok = [
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        format!("localhost:{port}"),
    ]
    .iter()
    .any(|allowed| allowed == host);
    if !host_ok {
        return Err(Response::text(403, "unexpected Host header"));
    }

    // A cross-origin page can still make the browser send a request; what it
    // must not do is one that acts. Same-origin requests send either no Origin
    // or exactly ours.
    if let Some(origin) = request.header("origin") {
        if !is_our_origin(origin, port) {
            return Err(Response::text(403, "cross-origin request refused"));
        }
    }
    Ok(())
}

/// Whether `origin` is one of the names this server answers to.
///
/// `null`, which a sandboxed frame or a `file:` page sends, is never ours.
pub fn is_our_origin(origin: &str, port: u16) -> bool {
    [
        format!("http://127.0.0.1:{port}"),
        format!("http://[::1]:{port}"),
        format!("http://localhost:{port}"),
    ]
    .iter()
    .any(|allowed| allowed == origin)
}

/// Whether a request carries the token.
///
/// From the `Authorization` header, or from the query for the few things a
/// browser fetches without letting a script add a header: images and the
/// event stream.
pub fn check_token(request: &Request, token: &str) -> Result<(), Response> {
    let presented = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| request.query.get("token").cloned())
        .unwrap_or_default();
    if !constant_time_eq(presented.as_bytes(), token.as_bytes()) {
        return Err(Response::text(401, "a valid token is required"));
    }
    Ok(())
}

/// Compare without leaking where two strings first differ.
///
/// The token and the sign-in codes are local, so this is belt and braces --
/// but a comparison that returns early is a habit worth not having.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The uid owning the client end of a loopback connection, from the text of
/// `/proc/net/tcp`.
///
/// Every local account can connect to a port on 127.0.0.1, and a Host header
/// is whatever the client says it is. The kernel's socket table is not: it
/// lists both ends of every IPv4 TCP connection with the uid that opened
/// each, and the file is readable by everyone. The row wanted is the
/// client's own, whose local address is the client's and whose remote
/// address is ours.
///
/// `None` for anything that cannot be matched -- no such row, a row that does
/// not parse, an IPv6 address -- which the caller treats as a stranger.
pub fn peer_uid(proc_net_tcp: &str, client: SocketAddr, server: SocketAddr) -> Option<u32> {
    let (SocketAddr::V4(client), SocketAddr::V4(server)) = (client, server) else {
        return None;
    };
    let client = (*client.ip(), client.port());
    let server = (*server.ip(), server.port());
    proc_net_tcp.lines().find_map(|row| {
        // sl, local_address, rem_address, st, tx:rx, tr:when, retrnsmt, uid
        let mut fields = row.split_whitespace();
        let local = parse_tcp_address(fields.nth(1)?)?;
        let remote = parse_tcp_address(fields.next()?)?;
        if local != client || remote != server {
            return None;
        }
        // What is left of an earlier connection on the same two ports,
        // listed with uid 0 because it no longer has an owner.
        if fields.next()? == TIME_WAIT {
            return None;
        }
        fields.nth(3)?.parse().ok()
    })
}

/// The state column's value for a closed connection waiting out its timer.
const TIME_WAIT: &str = "06";

/// One `ADDRESS:PORT` from `/proc/net/tcp`.
///
/// The kernel prints the address as the 32-bit word it holds, which is in
/// network order in memory, so its bytes come back out in native order; the
/// port it converts first, so that one reads as written.
fn parse_tcp_address(field: &str) -> Option<(Ipv4Addr, u16)> {
    let (address, port) = field.split_once(':')?;
    if address.len() != 8 || port.len() != 4 {
        return None;
    }
    let address = u32::from_str_radix(address, 16).ok()?;
    let port = u16::from_str_radix(port, 16).ok()?;
    Some((Ipv4Addr::from(address.to_ne_bytes()), port))
}
