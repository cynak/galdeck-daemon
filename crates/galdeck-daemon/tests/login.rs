//! Signing a browser in to the configuration UI with a one-time code.
//!
//! `galdeck ui` asks the control socket for a code and opens the page with
//! it; the page posts the code to `/api/login` and gets the token back. The
//! code passes through places other accounts can read -- the command lines of
//! xdg-open and the browser, the browser's history -- so what matters here is
//! everything the endpoint refuses, and that a code works exactly once.
//!
//! These run a real server on an ephemeral loopback port, so each connection
//! also passes the check that it was opened by this user.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use galdeck_core::wake_channel;
use galdeck_daemon::engine::{ControlMsg, ControlSender};
use galdeck_daemon::http::{HttpServer, LoginError, UiLogins};
use galdeck_daemon::ipc_server;
use galdeck_daemon::preview::Preview;
use galdeck_ipc::{Event, Response};

struct Ui {
    port: u16,
    logins: UiLogins,
    preview: Preview,
    control: ControlSender,
    /// What reached the engine. Nothing in these tests should.
    engine: Receiver<ControlMsg>,
    dir: std::path::PathBuf,
}

impl Ui {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("galdeck-login-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (waker, _wake_rx) = wake_channel();
        let (tx, engine) = channel();
        let control = ControlSender::new(tx, waker);
        let preview = Preview::new();
        let server = HttpServer::bind_with_token(
            0,
            control.clone(),
            preview.clone(),
            Arc::new(AtomicBool::new(false)),
            dir.join("galdeck-ui.token"),
        )
        .expect("binding to an ephemeral loopback port");
        let port = server.port();
        let logins = server.logins();
        // Left running: it ends with the test binary.
        std::thread::spawn(move || server.run());
        Self {
            port,
            logins,
            preview,
            control,
            engine,
            dir,
        }
    }

    fn code(&self) -> String {
        self.logins.mint(Duration::from_secs(60)).unwrap()
    }

    fn send(&self, raw: &str) -> (u16, String) {
        send(self.port, raw)
    }

    fn login_with(&self, body: &str, path: &str, change: &[(&str, Option<&str>)]) -> (u16, String) {
        login_with(self.port, body, path, change)
    }

    fn login(&self, code: &str) -> (u16, String) {
        login(self.port, code)
    }

    /// Whether a token opens the API, on a path the engine is not needed for.
    fn token_works(&self, token: &str) -> bool {
        let (status, _) = self.send(&format!(
            "GET /api/preview HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\n\r\n",
            self.port
        ));
        status == 200
    }

    fn nothing_reached_the_engine(&self) -> bool {
        matches!(self.engine.try_recv(), Err(TryRecvError::Empty))
    }
}

/// Send raw bytes and read the whole reply: its status and its body.
fn send(port: u16, raw: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    let status = reply
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {reply:?}"));
    let body = reply
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, body)
}

/// A sign-in as the page sends it, with `change` applied to the headers
/// first: each is `(name, Some(value))` to set one, `(name, None)` to leave
/// it out.
fn login_with(port: u16, body: &str, path: &str, change: &[(&str, Option<&str>)]) -> (u16, String) {
    let mut headers: Vec<(String, String)> = vec![
        ("Host".into(), format!("127.0.0.1:{port}")),
        ("Origin".into(), format!("http://127.0.0.1:{port}")),
        ("Sec-Fetch-Site".into(), "same-origin".into()),
        ("Content-Type".into(), "application/json".into()),
    ];
    for (name, value) in change {
        headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        if let Some(value) = value {
            headers.push((name.to_string(), value.to_string()));
        }
    }
    let mut raw = format!("POST {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    raw.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    send(port, &raw)
}

fn login(port: u16, code: &str) -> (u16, String) {
    login_with(port, &format!(r#"{{"code":"{code}"}}"#), "/api/login", &[])
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn token_in(body: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["token"].as_str().unwrap().to_string()
}

fn error_in(body: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["error"].as_str().unwrap().to_string()
}

#[test]
fn a_code_signs_in_once_and_then_says_it_was_used() {
    let ui = Ui::start("once");
    let code = ui.code();

    let (status, body) = ui.login(&code);
    assert_eq!(status, 200, "{body}");
    let token = token_in(&body);
    assert_eq!(token, ui.logins.token());
    assert!(ui.token_works(&token));

    // Said apart from "expired": if the person holding the link did not use
    // it, someone else did, and the page tells them what to do about that.
    let (status, body) = ui.login(&code);
    assert_eq!(status, 401);
    assert_eq!(error_in(&body), "used");
    assert!(ui.nothing_reached_the_engine());
}

#[test]
fn a_code_nobody_minted_is_unknown() {
    let ui = Ui::start("unknown");
    let _real = ui.code();
    for guess in ["00000000000000000000000000000000", "not hex at all", ""] {
        let (status, body) = ui.login(guess);
        assert_eq!(status, 401, "{guess:?}");
        assert_eq!(error_in(&body), "unknown", "{guess:?}");
    }
}

#[test]
fn a_code_past_its_time_is_expired() {
    let ui = Ui::start("expired");
    let code = ui.logins.mint(Duration::ZERO).unwrap();
    let (status, body) = ui.login(&code);
    assert_eq!(status, 401);
    assert_eq!(error_in(&body), "expired");
}

#[test]
fn codes_live_for_their_time_to_live_and_no_longer() {
    let ui = Ui::start("ttl");
    let now = Instant::now();
    let code = ui.logins.mint_at(now, Duration::from_secs(60)).unwrap();
    let later = ui.logins.mint_at(now, Duration::from_secs(60)).unwrap();
    assert!(ui
        .logins
        .redeem_at(&code, now + Duration::from_secs(59))
        .is_ok());
    assert_eq!(
        ui.logins.redeem_at(&later, now + Duration::from_secs(60)),
        Err(LoginError::Expired)
    );
}

#[test]
fn no_code_lives_longer_than_five_minutes_whatever_it_asked_for() {
    let ui = Ui::start("cap");
    let now = Instant::now();
    let code = ui.logins.mint_at(now, Duration::from_secs(86_400)).unwrap();
    assert_eq!(
        ui.logins.redeem_at(&code, now + Duration::from_secs(301)),
        Err(LoginError::Expired)
    );
}

#[test]
fn a_ninth_code_pushes_out_the_oldest_rather_than_being_refused() {
    // Refusing it would lock the user out of `galdeck ui` for a minute after
    // a few quick runs.
    let ui = Ui::start("evict");
    let now = Instant::now();
    let codes: Vec<String> = (0..9)
        .map(|_| ui.logins.mint_at(now, Duration::from_secs(60)).unwrap())
        .collect();
    assert_eq!(
        ui.logins.redeem_at(&codes[0], now),
        Err(LoginError::Unknown)
    );
    for code in &codes[1..] {
        assert!(ui.logins.redeem_at(code, now).is_ok());
    }
}

#[test]
fn two_redemptions_at_once_let_exactly_one_in() {
    let ui = Ui::start("race");
    let code = ui.code();
    let start = Arc::new(Barrier::new(4));
    let attempts: Vec<_> = (0..4)
        .map(|_| {
            let (port, code, start) = (ui.port, code.clone(), Arc::clone(&start));
            std::thread::spawn(move || {
                start.wait();
                login(port, &code).0
            })
        })
        .collect();
    let statuses: Vec<u16> = attempts.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 401).count(),
        3,
        "{statuses:?}"
    );
}

#[test]
fn opening_the_page_with_a_code_does_not_use_it_up() {
    // A browser prefetching the address, a link preview, a scanner: none of
    // them runs the script, so none of them may spend the code.
    let ui = Ui::start("get");
    let code = ui.code();
    for path in ["/", "/index.html"] {
        let (status, _) = ui.send(&format!(
            "GET {path}?code={code} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            ui.port
        ));
        assert_eq!(status, 200);
    }
    let (status, _) = ui.send(&format!(
        "GET /api/login?code={code} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        ui.port
    ));
    assert_eq!(status, 405);
    assert_eq!(ui.login(&code).0, 200);
}

#[test]
fn every_request_the_page_would_not_send_is_refused_without_spending_the_code() {
    let ui = Ui::start("refusals");
    let code = ui.code();
    let body = format!(r#"{{"code":"{code}"}}"#);
    let port = ui.port;
    let foreign_host = format!("evil.example:{port}");
    let other_port = format!("http://127.0.0.1:{}", port.wrapping_add(1));
    let big = format!(r#"{{"code":"{code}","pad":"{}"}}"#, "x".repeat(2048));
    let in_address = format!("/api/login?code={code}");

    let refused =
        |what: &str, expected: u16, body: &str, path: &str, change: &[(&str, Option<&str>)]| {
            let (status, reply) = ui.login_with(body, path, change);
            assert_eq!(status, expected, "{what}: {reply}");
        };
    let at = "/api/login";

    refused(
        "a rebound hostname",
        403,
        &body,
        at,
        &[("Host", Some(&foreign_host))],
    );
    refused("no Host at all", 403, &body, at, &[("Host", None)]);
    refused("no Origin", 403, &body, at, &[("Origin", None)]);
    refused("a null Origin", 403, &body, at, &[("Origin", Some("null"))]);
    refused(
        "another site's Origin",
        403,
        &body,
        at,
        &[("Origin", Some("https://example.com"))],
    );
    refused(
        "another port's Origin",
        403,
        &body,
        at,
        &[("Origin", Some(&other_port))],
    );
    refused(
        "a cross-site fetch",
        403,
        &body,
        at,
        &[("Sec-Fetch-Site", Some("cross-site"))],
    );
    refused(
        "a same-site fetch",
        403,
        &body,
        at,
        &[("Sec-Fetch-Site", Some("same-site"))],
    );
    let form = Some("application/x-www-form-urlencoded");
    refused(
        "a form's content type",
        415,
        &body,
        at,
        &[("Content-Type", form)],
    );
    refused(
        "plain text",
        415,
        &body,
        at,
        &[("Content-Type", Some("text/plain"))],
    );
    refused("no content type", 415, &body, at, &[("Content-Type", None)]);
    refused("the code in the address", 400, "{}", &in_address, &[]);
    refused("a body over a kilobyte", 413, &big, at, &[]);
    refused("a body with no code", 400, r#"{"token":"x"}"#, at, &[]);
    refused("a body that is not JSON", 400, &code, at, &[]);

    // A browser without Sec-Fetch-Site, and a charset on the type, are fine.
    let (status, reply) = ui.login_with(
        &body,
        "/api/login",
        &[
            ("Sec-Fetch-Site", None),
            ("Content-Type", Some("application/json; charset=utf-8")),
        ],
    );
    assert_eq!(status, 200, "none of the refusals spent the code: {reply}");
}

#[test]
fn only_a_post_signs_in() {
    let ui = Ui::start("methods");
    let code = ui.code();
    let body = format!(r#"{{"code":"{code}"}}"#);
    for method in ["GET", "PUT", "OPTIONS", "DELETE"] {
        let (status, _) = ui.send(&format!(
            "{method} /api/login HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
            port = ui.port
        ));
        assert_eq!(status, 405, "{method}");
    }
    assert_eq!(ui.login(&code).0, 200);
}

#[test]
fn the_page_cannot_mint_itself_a_code_through_the_api() {
    // /api/call forwards requests to the engine. A page that could ask for
    // codes, or sign every other tab out, would need no sign-in at all.
    let ui = Ui::start("api-call");
    let token = ui.logins.token();
    for call in [
        r#"{"cmd":"ui_login"}"#,
        r#"{"cmd":"ui_login","ttl_s":300}"#,
        r#"{"cmd":"ui_rotate_token"}"#,
    ] {
        let (status, body) = ui.send(&format!(
            "POST /api/call HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{call}",
            ui.port,
            call.len()
        ));
        assert_eq!(status, 403, "{call}: {body}");
    }
    assert!(ui.nothing_reached_the_engine());
    assert_eq!(ui.logins.token(), token);
}

#[test]
fn a_new_token_signs_out_the_old_one_and_forgets_every_code() {
    let ui = Ui::start("rotate");
    let old = ui.logins.token();
    let code = ui.code();
    assert!(ui.token_works(&old));

    ui.logins.rotate_token().unwrap();
    assert!(!ui.token_works(&old));
    assert!(ui.token_works(&ui.logins.token()));
    let (status, body) = ui.login(&code);
    assert_eq!(status, 401);
    assert_eq!(error_in(&body), "unknown");
}

#[test]
fn a_code_is_exactly_the_digits_that_were_minted() {
    // `+a` parses as a hex byte just as `0a` does; it is still not the code.
    let ui = Ui::start("exact");
    let now = Instant::now();
    let code = loop {
        let code = ui.logins.mint_at(now, Duration::from_secs(60)).unwrap();
        if code.starts_with('0') {
            break code;
        }
    };
    let signed = format!("+{}", &code[1..]);
    assert_eq!(ui.logins.redeem_at(&signed, now), Err(LoginError::Unknown));
    assert!(ui.logins.redeem_at(&code, now).is_ok());
}

#[test]
fn a_new_token_ends_an_event_stream_opened_with_the_old_one() {
    // A stream is authorised once, when it opens. A tab signed out by
    // `galdeck ui --new-token`, maybe one opened by whoever used a link
    // first, must not go on hearing every key press.
    let ui = Ui::start("stream");
    let mut stream = TcpStream::connect(("127.0.0.1", ui.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(
            format!(
                "GET /api/events?token={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                ui.logins.token(),
                ui.port
            )
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    while !head.ends_with("\r\n\r\n") {
        assert_ne!(reader.read_line(&mut head).unwrap(), 0, "{head}");
    }
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");

    ui.preview.publish(Event::KeyPressed { key: 3 });
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "event: key_pressed\n");

    ui.logins.rotate_token().unwrap();
    ui.preview.publish(Event::KeyPressed { key: 4 });
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    assert!(
        !rest.contains(r#""key":4"#),
        "sent after the new token: {rest}"
    );
}

#[test]
fn every_reply_asks_for_no_referrer() {
    let ui = Ui::start("referrer");
    let mut stream = TcpStream::connect(("127.0.0.1", ui.port)).unwrap();
    stream
        .write_all(format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", ui.port).as_bytes())
        .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    assert!(reply.contains("referrer-policy: no-referrer\r\n"));
}

/// Serve the control socket from a private directory, with or without the UI.
fn control_socket(ui: Option<&Ui>, name: &str) -> (UnixStream, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("galdeck-login-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("galdeck.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (control, logins) = match ui {
        Some(ui) => (ui.control.clone(), Some(ui.logins.clone())),
        None => {
            let (waker, _wake_rx) = wake_channel();
            let (tx, _rx) = channel();
            (ControlSender::new(tx, waker), None)
        }
    };
    std::thread::spawn(move || ipc_server::serve(listener, control, logins));
    (UnixStream::connect(&path).unwrap(), dir)
}

fn ask(stream: &mut UnixStream, request: &str) -> Response {
    stream.write_all(format!("{request}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn the_control_socket_hands_out_codes_that_sign_in() {
    let ui = Ui::start("socket");
    let (mut socket, dir) = control_socket(Some(&ui), "socket-ctl");

    let Response::UiLogin { port, code } = ask(&mut socket, r#"{"cmd":"ui_login"}"#) else {
        panic!("expected a code");
    };
    assert_eq!(port, ui.port);
    assert_eq!(code.len(), 32);
    assert!(code.chars().all(|c| c.is_ascii_hexdigit()));
    let (status, body) = ui.login(&code);
    assert_eq!(status, 200);
    assert_eq!(token_in(&body), ui.logins.token());

    let old = ui.logins.token();
    assert!(matches!(
        ask(&mut socket, r#"{"cmd":"ui_rotate_token"}"#),
        Response::Ok
    ));
    assert_ne!(ui.logins.token(), old);
    // Neither went anywhere near the engine.
    assert!(ui.nothing_reached_the_engine());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_daemon_without_the_ui_says_so() {
    let (mut socket, dir) = control_socket(None, "off-ctl");
    for request in [
        r#"{"cmd":"ui_login","ttl_s":60}"#,
        r#"{"cmd":"ui_rotate_token"}"#,
    ] {
        let Response::Error { message } = ask(&mut socket, request) else {
            panic!("expected an error for {request}");
        };
        // The CLI recognises this by its start.
        assert!(message.starts_with("the UI is off"), "{message}");
        assert_eq!(message, ipc_server::UI_OFF);
    }
    let _ = std::fs::remove_dir_all(dir);
}
