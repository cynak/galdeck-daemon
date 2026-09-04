//! Server-sent events.
//!
//! Chosen over WebSockets deliberately. The daemon is blocking threads with no
//! async runtime, and this direction of traffic is the only one that needs to
//! be a stream: the browser sends commands as ordinary requests. A WebSocket
//! would mean a handshake, a masking layer, fragmentation and control frames --
//! several hundred lines of security-relevant parsing -- to carry lines of text
//! one way. `text/event-stream` is a header and `data: ...\n\n`, and
//! `EventSource` reconnects on its own.

/// The response head that opens a stream.
pub fn open() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(b"content-type: text/event-stream; charset=utf-8\r\n");
    out.extend_from_slice(b"cache-control: no-store\r\n");
    // Without this a proxy can hold the stream in a buffer forever.
    out.extend_from_slice(b"x-accel-buffering: no\r\n");
    out.extend_from_slice(b"connection: keep-alive\r\n\r\n");
    out
}

/// Encode one event.
///
/// A payload containing newlines has to become several `data:` lines, or the
/// stream is silently truncated at the first one.
pub fn event(name: &str, data: &str) -> Vec<u8> {
    let mut out = String::with_capacity(data.len() + name.len() + 16);
    if !name.is_empty() {
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
    }
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.into_bytes()
}

/// A comment, which keeps an idle connection from being reaped without
/// delivering anything the client has to handle.
pub fn keepalive() -> Vec<u8> {
    b": keepalive\n\n".to_vec()
}
