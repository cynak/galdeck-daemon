//! Serving the configuration UI on loopback.
//!
//! Off unless asked for. This surface can set the shell commands the daemon
//! runs, so it binds 127.0.0.1 only, serves no connection that another local
//! account opened, requires a token, and refuses any request whose `Host` or
//! `Origin` is not our own — which is what stops a page the user happens to
//! visit from pointing a hostname at 127.0.0.1 and driving this API through
//! their browser.
//!
//! The token never appears in an address. `galdeck ui` asks the control
//! socket for a one-time code, opens the page with that, and the page trades
//! it here for the token. A code is good once, for a minute or five, so one
//! left in a browser's history, a terminal or a process list is worth nothing
//! by the time anyone reads it there.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use galdeck_http::{
    authorize, check_host_origin, check_token, constant_time_eq, is_our_origin, parse_request,
    peer_uid, sse, ParseError, Request as HttpRequest, Response,
};
use galdeck_ipc::{Request, Response as ControlResponse};

use crate::engine::{ControlMsg, ControlSender};
use crate::preview::Preview;

/// How long a client may take to send a complete request.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the engine to answer before giving up on a request.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// How often an idle event stream sends a comment, so a dead peer is noticed
/// and a proxy does not reap the connection.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);
/// Length of the UI token, in bytes before hex encoding.
const TOKEN_BYTES: usize = 24;
/// Length of a sign-in code, in bytes before hex encoding: 128 bits, which
/// nobody guesses in the minute it lives.
const CODE_BYTES: usize = 16;
/// How many codes may be outstanding at once. Minting another forgets the
/// oldest rather than refusing, so running `galdeck ui` again always works.
const MAX_CODES: usize = 8;
/// How many redeemed codes are remembered, so a second use of one can say
/// "already used" -- which is worth knowing, if it was not you -- rather than
/// "expired".
const MAX_USED: usize = 8;
/// How long a code lives when nothing else is asked for: long enough for a
/// browser to start.
pub const LOGIN_TTL: Duration = Duration::from_secs(60);
/// The longest a code may live, for one printed to be copied by hand.
pub const MAX_LOGIN_TTL: Duration = Duration::from_secs(300);
/// Largest sign-in request body. A code is 32 characters.
const MAX_LOGIN_BODY: usize = 1024;
/// Largest socket table read to find who is connecting. Past this the
/// connection is refused rather than looked up in part.
const MAX_TCP_TABLE: u64 = 16 * 1024 * 1024;

/// The embedded configuration UI.
const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
/// The Keyboard tab, a module app.js imports.
const KEYBOARD_JS: &str = include_str!("../ui/keyboard.js");
const APP_CSS: &str = include_str!("../ui/app.css");

pub struct HttpServer {
    listener: TcpListener,
    logins: UiLogins,
    port: u16,
    control: ControlSender,
    preview: Preview,
    shutdown: Arc<AtomicBool>,
}

impl HttpServer {
    /// Bind the UI server to loopback.
    pub fn bind(
        port: u16,
        control: ControlSender,
        preview: Preview,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        Self::bind_with_token(port, control, preview, shutdown, token_path())
    }

    /// Bind, keeping the token in a named file.
    pub fn bind_with_token(
        port: u16,
        control: ControlSender,
        preview: Preview,
        shutdown: Arc<AtomicBool>,
        token_file: PathBuf,
    ) -> Result<Self> {
        // Loopback only, and explicitly rather than by configuration: there is
        // no reason to expose this and every reason not to.
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = TcpListener::bind(address)
            .with_context(|| format!("binding the ui server to {address}"))?;
        let port = listener.local_addr()?.port();
        let token = load_or_create_token(&token_file)?;
        Ok(Self {
            listener,
            logins: UiLogins::new(port, token, token_file),
            port,
            control,
            preview,
            shutdown,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The sign-in codes and the token, shared with the control socket,
    /// which is what hands codes out.
    pub fn logins(&self) -> UiLogins {
        self.logins.clone()
    }

    pub fn run(self) {
        // The address alone. Under systemd this line goes to the journal,
        // which more than this user can read, so it must never carry a
        // token or a code.
        log::info!(
            "configuration UI on http://127.0.0.1:{}/ — run `galdeck ui` to open it",
            self.port
        );
        let mut refused_before = false;
        for stream in self.listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            match stream {
                Ok(stream) => {
                    // Before a thread or a byte of the request: whoever else
                    // is on this machine gets a closed connection and
                    // nothing else.
                    if let Err(why) = opened_by_this_user(&stream) {
                        if refused_before {
                            log::debug!("refused a ui connection: {why}");
                        } else {
                            log::warn!(
                                "refused a ui connection: {why} (the UI serves only the user \
                                 the daemon runs as; any more are logged at debug level)"
                            );
                            refused_before = true;
                        }
                        continue;
                    }
                    let logins = self.logins.clone();
                    let port = self.port;
                    let control = self.control.clone();
                    let preview = self.preview.clone();
                    let shutdown = Arc::clone(&self.shutdown);
                    // A thread per connection, as everywhere else in this
                    // daemon. There is one browser.
                    std::thread::Builder::new()
                        .name("galdeck-http".into())
                        .spawn(move || {
                            if let Err(e) =
                                serve_one(stream, &logins, port, &control, &preview, &shutdown)
                            {
                                log::debug!("ui connection ended: {e}");
                            }
                        })
                        .ok();
                }
                Err(e) => {
                    log::warn!("ui accept failed: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// Whether the client end of a connection belongs to the user this daemon
/// runs as, and if not, why not.
///
/// Loopback is reachable by every account on the machine -- service
/// accounts, other people, a container on the host network -- and nothing a
/// client sends says who it is. The kernel's socket table does. Anything
/// that cannot be looked up is a stranger.
fn opened_by_this_user(stream: &TcpStream) -> Result<(), String> {
    let (client, server) = match (stream.peer_addr(), stream.local_addr()) {
        (Ok(client), Ok(server)) => (client, server),
        (Err(e), _) | (_, Err(e)) => return Err(format!("its address is unknown: {e}")),
    };
    let mut table = String::new();
    std::fs::File::open("/proc/net/tcp")
        .and_then(|file| file.take(MAX_TCP_TABLE).read_to_string(&mut table))
        .map_err(|e| format!("reading /proc/net/tcp to see who {client} is: {e}"))?;
    // Safety: getuid cannot fail and touches no memory.
    let ours = unsafe { libc::getuid() };
    match peer_uid(&table, client, server) {
        Some(uid) if uid == ours => Ok(()),
        Some(uid) => Err(format!("{client} belongs to uid {uid}, not {ours}")),
        None => Err(format!("{client} is not in /proc/net/tcp")),
    }
}

/// Why a sign-in code was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    /// It worked once already. Worth saying apart from the others: if the
    /// person holding it did not use it, someone else did.
    Used,
    /// It was real, and it ran out.
    Expired,
    /// Never minted, pushed out by newer codes, or mistyped.
    Unknown,
}

impl LoginError {
    /// The word the page is told, in `{"error": ...}`.
    pub fn as_str(self) -> &'static str {
        match self {
            LoginError::Used => "used",
            LoginError::Expired => "expired",
            LoginError::Unknown => "unknown",
        }
    }
}

/// The UI's token and the one-time codes that lead to it.
///
/// Shared between the HTTP server, which redeems codes, and the control
/// socket, which mints them and rotates the token. The engine never sees
/// either.
#[derive(Clone)]
pub struct UiLogins {
    inner: Arc<Mutex<Logins>>,
}

struct Logins {
    port: u16,
    token: String,
    token_file: PathBuf,
    /// Oldest first, which is the order they are forgotten in.
    codes: Vec<Code>,
    /// Codes already redeemed, oldest first.
    used: Vec<[u8; CODE_BYTES]>,
}

struct Code {
    bytes: [u8; CODE_BYTES],
    minted: Instant,
    ttl: Duration,
}

impl UiLogins {
    fn new(port: u16, token: String, token_file: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Logins {
                port,
                token,
                token_file,
                codes: Vec::new(),
                used: Vec::new(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Logins> {
        // Nothing under this lock can panic halfway through a change.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The port the UI is served on.
    pub fn port(&self) -> u16 {
        self.lock().port
    }

    /// The token the API takes now.
    pub fn token(&self) -> String {
        self.lock().token.clone()
    }

    /// A new code, good once for `ttl` (at most [`MAX_LOGIN_TTL`]).
    pub fn mint(&self, ttl: Duration) -> Result<String> {
        self.mint_at(Instant::now(), ttl)
    }

    /// [`Self::mint`] at a given moment, for tests.
    pub fn mint_at(&self, now: Instant, ttl: Duration) -> Result<String> {
        // Straight from the kernel, and no code at all if that fails: a
        // weaker source would make a guessable code, which is worse than
        // none.
        let code = generate_token(CODE_BYTES)?;
        let bytes = decode_code(&code).context("a freshly minted code did not decode")?;
        let mut logins = self.lock();
        if logins.codes.len() >= MAX_CODES {
            logins.codes.remove(0);
        }
        logins.codes.push(Code {
            bytes,
            minted: now,
            ttl: ttl.min(MAX_LOGIN_TTL),
        });
        Ok(code)
    }

    /// Trade a code for the token, using it up.
    pub fn redeem(&self, code: &str) -> Result<String, LoginError> {
        self.redeem_at(code, Instant::now())
    }

    /// [`Self::redeem`] at a given moment, for tests.
    pub fn redeem_at(&self, code: &str, now: Instant) -> Result<String, LoginError> {
        let presented = decode_code(code).ok_or(LoginError::Unknown)?;
        let mut logins = self.lock();
        // Against every outstanding code, all the way to the end, so how
        // long this takes says nothing about which one nearly matched.
        let mut found = None;
        for (index, outstanding) in logins.codes.iter().enumerate() {
            if constant_time_eq(&outstanding.bytes, &presented) && found.is_none() {
                found = Some(index);
            }
        }
        let Some(index) = found else {
            let used = logins
                .used
                .iter()
                .fold(false, |seen, old| constant_time_eq(old, &presented) | seen);
            return Err(if used {
                LoginError::Used
            } else {
                LoginError::Unknown
            });
        };
        // Gone from the list whatever happens next: a code is presented once.
        let code = logins.codes.remove(index);
        if now.duration_since(code.minted) >= code.ttl {
            return Err(LoginError::Expired);
        }
        if logins.used.len() >= MAX_USED {
            logins.used.remove(0);
        }
        logins.used.push(code.bytes);
        Ok(logins.token.clone())
    }

    /// Replace the token with a new one, on disk and here, and forget every
    /// outstanding code.
    ///
    /// For when a token may have got out. Every open tab is signed out by
    /// it, which is the point; the codes go too, since each would lead to
    /// the new token.
    pub fn rotate_token(&self) -> Result<()> {
        let token = generate_token(TOKEN_BYTES)?;
        let mut logins = self.lock();
        write_token(&logins.token_file, &token)?;
        logins.token = token;
        logins.codes.clear();
        Ok(())
    }
}

/// A code as the page sends it back: 32 hex digits.
fn decode_code(code: &str) -> Option<[u8; CODE_BYTES]> {
    let digits = code.as_bytes();
    // Checked here because `from_str_radix` also takes a leading `+`, which
    // would let "+a" stand in for "0a": a code is exactly what was minted.
    if digits.len() != CODE_BYTES * 2 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut bytes = [0u8; CODE_BYTES];
    for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

/// Where the UI token is kept, given the control socket it sits beside.
///
/// Derived from the socket in use, so a daemon started with `--socket` keeps
/// its token beside its own socket rather than sharing one.
pub fn token_path_for(socket: &Path) -> PathBuf {
    socket
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("galdeck-ui.token")
}

/// Delete the token file daemons wrote before sign-in codes.
///
/// Those printed the address with the token in it at every start, and under
/// systemd that went to the journal, which the `adm` group can read. The
/// token now lives under another name, so the first start after the upgrade
/// mints a new one; this removes the old file, whose token nothing takes any
/// more.
pub fn remove_legacy_token(socket: &Path) {
    let old = socket
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("galdeck-ui-token");
    match std::fs::remove_file(&old) {
        Ok(()) => log::info!(
            "removed {}: older daemons wrote its token to the log, so the UI gets a new one",
            old.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("could not remove the old UI token {}: {e}", old.display()),
    }
}

fn token_path() -> PathBuf {
    token_path_for(&galdeck_ipc::socket_path())
}

/// Reuse the token from a previous run, or mint one.
///
/// A fresh token every run means every open browser tab is holding a dead one
/// the moment the daemon restarts -- and since the token is stripped from the
/// address bar on load, a refresh cannot recover it either. Keeping it in a
/// file the user owns and nobody else can read makes restarting the daemon a
/// non-event for a tab that is already open.
fn load_or_create_token(path: &Path) -> Result<String> {
    if let Some(existing) = read_token(path) {
        return Ok(existing);
    }
    let token = generate_token(TOKEN_BYTES)?;
    write_token(path, &token)?;
    Ok(token)
}

/// Write a token where [`read_token`] will accept it.
fn write_token(path: &Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Removed first, because `mode` only applies to a file this call creates.
    // Opening an existing world-readable file with `.mode(0o600)` silently
    // keeps its old permissions and writes the new token into it -- which
    // would leak exactly the token we replaced it for leaking.
    let _ = std::fs::remove_file(path);
    // Created 0600 rather than created and then chmodded, so there is no
    // window in which it is readable by anyone else.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing the ui token to {}", path.display()))?;
    file.write_all(token.as_bytes())?;
    Ok(())
}

/// Read a stored token, if it is one we would have written.
///
/// Anything else -- wrong owner, readable by others, not the right shape -- is
/// replaced rather than trusted.
fn read_token(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    // Safety: getuid cannot fail and touches no memory.
    if meta.uid() != unsafe { libc::getuid() } {
        log::warn!("{} is owned by someone else; replacing it", path.display());
        return None;
    }
    if meta.mode() & 0o077 != 0 {
        log::warn!("{} is readable by others; replacing it", path.display());
        return None;
    }
    let token = std::fs::read_to_string(path).ok()?.trim().to_string();
    let looks_right =
        token.len() == TOKEN_BYTES * 2 && token.chars().all(|c| c.is_ascii_hexdigit());
    looks_right.then_some(token)
}

/// `length` random bytes, as hex, with enough entropy that guessing is not a
/// strategy: the token, and the sign-in codes.
///
/// Read straight from the kernel rather than taking a dependency for it:
/// these are the only random numbers the daemon needs.
fn generate_token(length: usize) -> Result<String> {
    let mut bytes = vec![0u8; length];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom for the ui")?
        .read_exact(&mut bytes)
        .context("reading /dev/urandom for the ui")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn serve_one(
    mut stream: TcpStream,
    logins: &UiLogins,
    port: u16,
    control: &ControlSender,
    preview: &Preview,
    shutdown: &AtomicBool,
) -> Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(response) => {
            let _ = stream.write_all(&response.to_bytes());
            return Ok(());
        }
    };

    // Signing in is how a page gets the token, so it cannot need one; it
    // makes its own checks instead, starting with the Host and Origin ones
    // the public paths below skip.
    if request.path == "/api/login" {
        stream.write_all(&login(&request, port, logins).to_bytes())?;
        return Ok(());
    }

    // The page and its assets are served without a token. The page is what
    // signs in, and the browser asks for the script and stylesheet with a
    // plain `<script src>` and `<link href>` -- it has no way to attach a
    // header or a query to those.
    //
    // Nothing is given away by it: these are the same bytes for everyone,
    // compiled into the binary, containing no configuration and no state.
    // Everything that reads or changes anything is under /api and needs the
    // token. Serving the page never looks at a code in its address, either
    // -- only the script does, and it posts it -- so nothing a browser does
    // by itself, like prefetching an address, can use one up.
    let public = matches!(
        request.path.as_str(),
        "/" | "/index.html" | "/app.js" | "/keyboard.js" | "/app.css"
    );
    if !public {
        if let Err(response) = authorize(&request, &logins.token(), port) {
            let _ = stream.write_all(&response.to_bytes());
            return Ok(());
        }
    }

    // The event stream owns the connection for its lifetime.
    if request.path == "/api/events" {
        return stream_events(stream, &request, logins, preview, shutdown);
    }

    let response = route(&request, control, preview);
    stream.write_all(&response.to_bytes())?;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, Response> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        match parse_request(&buffer) {
            Ok(request) => return Ok(request),
            Err(ParseError::Incomplete) => {}
            Err(ParseError::BodyTooLarge) => return Err(Response::text(413, "that is too large")),
            Err(ParseError::TooLarge(what)) => {
                return Err(Response::text(413, format!("{what} too large")))
            }
            Err(ParseError::Malformed(why)) => {
                return Err(Response::text(400, format!("malformed request: {why}")))
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(Response::text(400, "connection closed mid-request")),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(Response::text(400, format!("read failed: {e}"))),
        }
    }
}

/// `POST /api/login`: trade a one-time code for the token.
///
/// Everything a page on another origin, a sandboxed frame, or a request not
/// made by a script could get wrong is refused before the code is looked at.
fn login(request: &HttpRequest, port: u16, logins: &UiLogins) -> Response {
    if request.method != "POST" {
        return Response::text(405, "method not allowed").with_header("allow", "POST");
    }
    if let Err(response) = check_host_origin(request, port) {
        return response;
    }
    // A browser sends Origin with every POST, same-origin ones included, so
    // one without it did not come from our page. `null` is a sandboxed frame
    // or a file, and is refused along with anything else not ours.
    if !request
        .header("origin")
        .is_some_and(|origin| is_our_origin(origin, port))
    {
        return Response::text(403, "signing in needs this page's own Origin");
    }
    if request
        .header("sec-fetch-site")
        .is_some_and(|site| site != "same-origin")
    {
        return Response::text(403, "cross-site request refused");
    }
    // Not a type a plain form can send, so a form on another page cannot
    // post here without the preflight it would fail.
    let json = request
        .header("content-type")
        .and_then(|value| value.split(';').next())
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Response::text(415, "send the code as application/json");
    }
    // In the body only: an address ends up in history and in logs.
    if request.query.contains_key("code") {
        return Response::text(400, "the code goes in the body, not the address");
    }
    if request.body.len() > MAX_LOGIN_BODY {
        return Response::text(413, "that is too large for a code");
    }
    let code = serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|body| body.get("code")?.as_str().map(str::to_string));
    let Some(code) = code else {
        return Response::text(400, r#"expected {"code": "..."}"#);
    };
    match logins.redeem(&code) {
        Ok(token) => Response::json(200, serde_json::json!({ "token": token }).to_string()),
        Err(problem) => Response::json(
            401,
            serde_json::json!({ "error": problem.as_str() }).to_string(),
        ),
    }
}

fn route(request: &HttpRequest, control: &ControlSender, preview: &Preview) -> Response {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => Response::html(INDEX_HTML),
        ("GET", "/app.js") => Response::new(200, "text/javascript; charset=utf-8", APP_JS.into()),
        ("GET", "/keyboard.js") => {
            Response::new(200, "text/javascript; charset=utf-8", KEYBOARD_JS.into())
        }
        ("GET", "/app.css") => Response::new(200, "text/css; charset=utf-8", APP_CSS.into()),

        ("POST", "/api/call") => {
            let Some(body) = request.body_str() else {
                return Response::text(400, "body is not utf-8");
            };
            let call: Request = match serde_json::from_str(body) {
                Ok(call) => call,
                Err(e) => return Response::text(400, format!("bad request: {e}")),
            };
            // A page that could mint its own sign-in codes, or sign every
            // other tab out, would not need signing in. The control socket
            // is the only place that answers these.
            if matches!(call, Request::UiLogin { .. } | Request::UiRotateToken) {
                return Response::text(403, "only answered on the control socket");
            }
            match ask(control, call) {
                Ok(response) => match serde_json::to_vec(&response) {
                    Ok(json) => Response::json(200, json),
                    Err(e) => Response::text(500, format!("encoding the reply: {e}")),
                },
                Err(message) => Response::text(500, message),
            }
        }

        // The preview is the same JPEG the panel was sent, so what the browser
        // shows cannot drift from what the hardware shows.
        ("GET", path) if path.starts_with("/api/preview/key/") => {
            let index = path
                .trim_start_matches("/api/preview/key/")
                .trim_end_matches(".jpg")
                .parse::<u8>();
            match index.ok().and_then(|index| preview.key(index)) {
                Some(jpeg) => Response::new(200, "image/jpeg", jpeg.to_vec()),
                None => Response::empty(404),
            }
        }
        ("GET", "/api/preview/lcd.jpg") => match preview.lcd() {
            Some(jpeg) => Response::new(200, "image/jpeg", jpeg.to_vec()),
            None => Response::empty(404),
        },
        ("GET", "/api/preview") => {
            let frame = preview.frame();
            let rings: Vec<Vec<String>> = frame
                .rings
                .iter()
                .map(|ring| ring.iter().map(hex).collect())
                .collect();
            let json = format!(
                r#"{{"generation":{},"brightness":{},"rings":{}}}"#,
                frame.generation,
                frame.brightness,
                serde_json::to_string(&rings).unwrap_or_else(|_| "[]".into())
            );
            Response::json(200, json)
        }

        // Compiled in, so answered here rather than by the engine.
        ("GET", "/api/logos") => Response::json(200, crate::logos::catalogue_json()),

        ("GET", _) => Response::text(404, "no such thing here"),
        _ => Response::text(405, "method not allowed"),
    }
}

fn hex(color: &galdeck::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// Put a request to the engine and wait for its answer.
fn ask(control: &ControlSender, request: Request) -> Result<ControlResponse, String> {
    // A download waits on someone else's server rather than on the engine.
    let wait = match request {
        Request::FetchAsset { .. } => crate::assets::DOWNLOAD_TIMEOUT + Duration::from_secs(1),
        _ => CONTROL_TIMEOUT,
    };
    let (reply, rx) = channel();
    control
        .send(ControlMsg { request, reply })
        .map_err(|_| "the daemon is shutting down".to_string())?;
    rx.recv_timeout(wait)
        .map_err(|_| "the daemon did not answer in time".to_string())
}

/// Send events down `stream` until the client goes, the daemon stops, or
/// the token that opened it is replaced.
///
/// The token is checked once when the stream opens and again before every
/// write, because a stream outlives any one request: without that, a tab
/// signed out by `galdeck ui --new-token` -- perhaps one somebody else opened
/// with a stolen link -- would go on hearing every key press.
fn stream_events(
    mut stream: TcpStream,
    request: &HttpRequest,
    logins: &UiLogins,
    preview: &Preview,
    shutdown: &AtomicBool,
) -> Result<()> {
    let events = preview.subscribe();
    stream.write_all(&sse::open())?;
    stream.flush()?;

    while !shutdown.load(Ordering::Relaxed) {
        let next = events.recv_timeout(SSE_KEEPALIVE);
        if check_token(request, &logins.token()).is_err() {
            break;
        }
        match next {
            Ok(event) => {
                let name = event_name(&event);
                let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                stream.write_all(&sse::event(name, &data))?;
                stream.flush()?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Writing this is also how a vanished client is noticed.
                stream.write_all(&sse::keepalive())?;
                stream.flush()?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn event_name(event: &galdeck_ipc::Event) -> &'static str {
    use galdeck_ipc::Event::*;
    match event {
        DeviceConnected { .. } => "device_connected",
        DeviceDisconnected => "device_disconnected",
        PageChanged { .. } => "page_changed",
        ProfileChanged { .. } => "profile_changed",
        BrightnessChanged { .. } => "brightness_changed",
        KeyPressed { .. } => "key_pressed",
        EncoderTurned { .. } => "encoder_turned",
        EncoderPressed { .. } => "encoder_pressed",
        ConfigChanged => "config_changed",
        DeviceReleased => "device_released",
        DeviceResumed => "device_resumed",
        CalibrationChanged => "calibration_changed",
        TimerDone { .. } => "timer_done",
        ModeChanged { .. } => "mode_changed",
        KeyStateChanged { .. } => "key_state_changed",
    }
}
