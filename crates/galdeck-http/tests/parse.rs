//! Parsing bytes off a socket, tested without a socket.

use galdeck_http::{parse_request, ParseError, Response, CONTENT_SECURITY_POLICY};

fn req(raw: &str) -> Result<galdeck_http::Request, ParseError> {
    parse_request(raw.as_bytes())
}

#[test]
fn parses_a_plain_get() {
    let r = req("GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:8787\r\n\r\n").unwrap();
    assert_eq!(r.method, "GET");
    assert_eq!(r.path, "/api/status");
    assert_eq!(r.header("host"), Some("127.0.0.1:8787"));
    assert!(r.body.is_empty());
}

#[test]
fn header_names_are_case_insensitive() {
    // Forgetting this is a reliable way to bypass a header check.
    let r = req("GET / HTTP/1.1\r\nHOST: x\r\nOrIgIn: y\r\n\r\n").unwrap();
    assert_eq!(r.header("host"), Some("x"));
    assert_eq!(r.header("origin"), Some("y"));
    assert_eq!(r.header("ORIGIN"), Some("y"));
}

#[test]
fn splits_the_query_off_the_path() {
    let r = req("GET /api/events?token=abc&topics=page HTTP/1.1\r\n\r\n").unwrap();
    assert_eq!(r.path, "/api/events");
    assert_eq!(r.query.get("token").map(String::as_str), Some("abc"));
    assert_eq!(r.query.get("topics").map(String::as_str), Some("page"));
}

#[test]
fn decodes_percent_escapes_in_paths_and_queries() {
    let r = req("GET /api/file/profiles%2Fwork.toml?name=a%20b HTTP/1.1\r\n\r\n").unwrap();
    assert_eq!(r.path, "/api/file/profiles/work.toml");
    assert_eq!(r.query.get("name").map(String::as_str), Some("a b"));
}

#[test]
fn a_malformed_escape_is_left_alone_rather_than_dropped() {
    // Dropping it would silently turn one path into a different path.
    let r = req("GET /a%zz HTTP/1.1\r\n\r\n").unwrap();
    assert_eq!(r.path, "/a%zz");
}

#[test]
fn reads_a_body_of_the_declared_length() {
    let r = req("POST /api/call HTTP/1.1\r\nContent-Length: 7\r\n\r\n{\"a\":1}").unwrap();
    assert_eq!(r.body_str(), Some("{\"a\":1}"));
}

#[test]
fn a_truncated_request_asks_for_more_rather_than_failing() {
    assert_eq!(
        req("GET / HTTP/1.1\r\nHost: x"),
        Err(ParseError::Incomplete)
    );
    assert_eq!(
        req("POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort"),
        Err(ParseError::Incomplete)
    );
}

#[test]
fn an_oversized_body_is_refused_before_it_is_read() {
    let raw = format!(
        "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        galdeck_http::MAX_BODY + 1
    );
    assert_eq!(req(&raw), Err(ParseError::BodyTooLarge));
}

#[test]
fn endless_headers_are_refused_rather_than_buffered_forever() {
    let mut raw = String::from("GET / HTTP/1.1\r\n");
    while raw.len() < galdeck_http::MAX_REQUEST_LINE + galdeck_http::MAX_HEADERS + 1 {
        raw.push_str("x: y\r\n");
    }
    assert!(matches!(
        parse_request(raw.as_bytes()),
        Err(ParseError::TooLarge(_))
    ));
}

#[test]
fn rejects_nonsense() {
    assert!(matches!(
        req("GET / SPDY/9\r\n\r\n"),
        Err(ParseError::Malformed(_))
    ));
    assert!(matches!(
        req("GET / HTTP/1.1\r\nno-colon\r\n\r\n"),
        Err(ParseError::Malformed(_))
    ));
}

#[test]
fn a_response_serializes_with_a_length_and_the_hardening_headers() {
    let bytes = Response::json(200, "{}").to_bytes();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(text.contains("content-length: 2\r\n"));
    assert!(text.contains("content-type: application/json; charset=utf-8\r\n"));
    assert!(text.contains("x-content-type-options: nosniff\r\n"));
    assert!(text.contains("x-frame-options: DENY\r\n"));
    assert!(text.contains("cache-control: no-store\r\n"));
    assert!(text.ends_with("\r\n\r\n{}"));

    // Written out in full, so loosening the policy is a change to this test
    // and not something that happens along the way.
    assert_eq!(
        CONTENT_SECURITY_POLICY,
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
         connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    let script_src = CONTENT_SECURITY_POLICY
        .split(';')
        .map(str::trim)
        .find(|directive| directive.starts_with("script-src "))
        .expect("the policy names where script may come from");
    assert!(!script_src.contains("'unsafe-inline'"), "{script_src}");
    assert!(!script_src.contains("'unsafe-eval'"), "{script_src}");

    // Errors carry it too: they are what a browser shows for a stale link.
    let header = format!("content-security-policy: {CONTENT_SECURITY_POLICY}\r\n");
    for response in [
        Response::json(200, "{}"),
        Response::html("<p>hi</p>"),
        Response::text(401, "a valid token is required"),
        Response::empty(204),
    ] {
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.contains(&header), "{text}");
    }
}
