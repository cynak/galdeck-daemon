//! Serving the configuration UI on loopback.
//!
//! Off unless asked for. This surface can set the shell commands the daemon
//! runs, so it binds 127.0.0.1 only, requires a token generated at startup,
//! and refuses any request whose `Host` or `Origin` is not our own — which is
//! what stops a page the user happens to visit from pointing a hostname at
//! 127.0.0.1 and driving this API through their browser.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use galdeck_http::{authorize, parse_request, sse, ParseError, Request as HttpRequest, Response};
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

/// The embedded configuration UI.
const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");

pub struct HttpServer {
    listener: TcpListener,
    token: String,
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
        Ok(Self {
            listener,
            token: load_or_create_token(&token_file)?,
            port,
            control,
            preview,
            shutdown,
        })
    }

    /// The address to open, token included.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?token={}", self.port, self.token)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn run(self) {
        log::info!("configuration UI at {}", self.url());
        for stream in self.listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            match stream {
                Ok(stream) => {
                    let token = self.token.clone();
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
                                serve_one(stream, &token, port, &control, &preview, &shutdown)
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

/// Where the UI token is kept, given the control socket it sits beside.
///
/// Public so the CLI can find it without asking the daemon -- which matters
/// precisely when the browser's token has gone stale and the daemon is the
/// thing refusing to talk.
pub fn token_path_for(socket: &std::path::Path) -> PathBuf {
    socket
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/tmp"))
        .join("galdeck-ui-token")
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
fn load_or_create_token(path: &std::path::Path) -> Result<String> {
    if let Some(existing) = read_token(path) {
        return Ok(existing);
    }
    let token = generate_token()?;
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
    Ok(token)
}

/// Read a stored token, if it is one we would have written.
///
/// Anything else -- wrong owner, readable by others, not the right shape -- is
/// replaced rather than trusted.
fn read_token(path: &std::path::Path) -> Option<String> {
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

/// A token with enough entropy that guessing it is not a strategy.
///
/// Read straight from the kernel rather than taking a dependency for it: this
/// is the only random number the daemon needs.
fn generate_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom for the ui token")?
        .read_exact(&mut bytes)
        .context("reading the ui token")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn serve_one(
    mut stream: TcpStream,
    token: &str,
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

    // The page and its assets are served without a token. The index is how the
    // token reaches the browser in the first place, and the browser then asks
    // for the script and stylesheet with a plain `<script src>` and
    // `<link href>` -- it has no way to attach a header or a query to those.
    //
    // Nothing is given away by it: these are the same bytes for everyone,
    // compiled into the binary, containing no configuration and no state.
    // Everything that reads or changes anything is under /api and needs the
    // token.
    let public = matches!(
        request.path.as_str(),
        "/" | "/index.html" | "/app.js" | "/app.css"
    );
    if !public {
        if let Err(response) = authorize(&request, token, port) {
            let _ = stream.write_all(&response.to_bytes());
            return Ok(());
        }
    }

    // The event stream owns the connection for its lifetime.
    if request.path == "/api/events" {
        return stream_events(stream, preview, shutdown);
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

fn route(request: &HttpRequest, control: &ControlSender, preview: &Preview) -> Response {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => Response::html(INDEX_HTML),
        ("GET", "/app.js") => Response::new(200, "text/javascript; charset=utf-8", APP_JS.into()),
        ("GET", "/app.css") => Response::new(200, "text/css; charset=utf-8", APP_CSS.into()),

        ("POST", "/api/call") => {
            let Some(body) = request.body_str() else {
                return Response::text(400, "body is not utf-8");
            };
            let call: Request = match serde_json::from_str(body) {
                Ok(call) => call,
                Err(e) => return Response::text(400, format!("bad request: {e}")),
            };
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

        ("GET", _) => Response::text(404, "no such thing here"),
        _ => Response::text(405, "method not allowed"),
    }
}

fn hex(color: &galdeck::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// Put a request to the engine and wait for its answer.
fn ask(control: &ControlSender, request: Request) -> Result<ControlResponse, String> {
    let (reply, rx) = channel();
    control
        .send(ControlMsg { request, reply })
        .map_err(|_| "the daemon is shutting down".to_string())?;
    rx.recv_timeout(CONTROL_TIMEOUT)
        .map_err(|_| "the daemon did not answer in time".to_string())
}

fn stream_events(mut stream: TcpStream, preview: &Preview, shutdown: &AtomicBool) -> Result<()> {
    let events = preview.subscribe();
    stream.write_all(&sse::open())?;
    stream.flush()?;

    while !shutdown.load(Ordering::Relaxed) {
        match events.recv_timeout(SSE_KEEPALIVE) {
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
    }
}
