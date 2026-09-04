//! The whole security model of the HTTP surface, tested by name.
//!
//! This API can set the shell commands the daemon runs. An unauthenticated
//! port on this machine would be a remote code execution hole reachable from
//! any web page the user happens to visit, so each defence gets a test that
//! says what it is for.

use galdeck_http::{authorize, parse_request, Request};

const TOKEN: &str = "s3cret-token-value";
const PORT: u16 = 8787;

fn request(headers: &str) -> Request {
    parse_request(format!("GET /api/status HTTP/1.1\r\n{headers}\r\n").as_bytes()).unwrap()
}

#[test]
fn a_correct_token_in_a_header_is_accepted() {
    let r = request(&format!(
        "Host: 127.0.0.1:{PORT}\r\nAuthorization: Bearer {TOKEN}\r\n"
    ));
    assert!(authorize(&r, TOKEN, PORT).is_ok());
}

#[test]
fn a_correct_token_in_the_query_is_accepted() {
    // EventSource cannot set headers, so the stream has to carry it here.
    let r = parse_request(
        format!("GET /api/events?token={TOKEN} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\r\n")
            .as_bytes(),
    )
    .unwrap();
    assert!(authorize(&r, TOKEN, PORT).is_ok());
}

#[test]
fn no_token_is_refused() {
    let r = request(&format!("Host: 127.0.0.1:{PORT}\r\n"));
    let err = authorize(&r, TOKEN, PORT).unwrap_err();
    assert_eq!(err.status, 401);
}

#[test]
fn a_wrong_token_is_refused() {
    let r = request(&format!(
        "Host: 127.0.0.1:{PORT}\r\nAuthorization: Bearer nope\r\n"
    ));
    assert_eq!(authorize(&r, TOKEN, PORT).unwrap_err().status, 401);
}

#[test]
fn a_token_that_is_a_prefix_of_the_real_one_is_refused() {
    let r = request(&format!(
        "Host: 127.0.0.1:{PORT}\r\nAuthorization: Bearer s3cret\r\n"
    ));
    assert_eq!(authorize(&r, TOKEN, PORT).unwrap_err().status, 401);
}

#[test]
fn a_rebound_hostname_is_refused_even_with_the_right_token() {
    // DNS rebinding: a site the user visits points evil.example at 127.0.0.1
    // and its script fetches this API. The browser sends the attacker's
    // hostname in Host, which is the tell.
    let r = request(&format!(
        "Host: evil.example:{PORT}\r\nAuthorization: Bearer {TOKEN}\r\n"
    ));
    let err = authorize(&r, TOKEN, PORT).unwrap_err();
    assert_eq!(err.status, 403);
}

#[test]
fn a_missing_host_is_refused() {
    let r = request(&format!("Authorization: Bearer {TOKEN}\r\n"));
    assert_eq!(authorize(&r, TOKEN, PORT).unwrap_err().status, 403);
}

#[test]
fn a_cross_origin_request_is_refused_even_with_the_right_token() {
    let r = request(&format!(
        "Host: 127.0.0.1:{PORT}\r\nOrigin: https://example.com\r\nAuthorization: Bearer {TOKEN}\r\n"
    ));
    let err = authorize(&r, TOKEN, PORT).unwrap_err();
    assert_eq!(err.status, 403);
}

#[test]
fn our_own_origin_is_accepted() {
    for origin in [
        format!("http://127.0.0.1:{PORT}"),
        format!("http://localhost:{PORT}"),
    ] {
        let r = request(&format!(
            "Host: 127.0.0.1:{PORT}\r\nOrigin: {origin}\r\nAuthorization: Bearer {TOKEN}\r\n"
        ));
        assert!(authorize(&r, TOKEN, PORT).is_ok(), "{origin} should pass");
    }
}

#[test]
fn a_request_on_the_wrong_port_is_refused() {
    // Another local service on a different port cannot borrow our token by
    // getting the browser to send it somewhere else.
    let r = request(&format!(
        "Host: 127.0.0.1:9999\r\nAuthorization: Bearer {TOKEN}\r\n"
    ));
    assert_eq!(authorize(&r, TOKEN, PORT).unwrap_err().status, 403);
}
