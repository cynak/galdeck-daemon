use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use galdeck_ipc::{Request, Response};

/// Control the galdeck daemon (Corsair Galleon 100 SD Stream Deck module).
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Control socket path (default: $GALDECK_SOCKET, else
    /// $XDG_RUNTIME_DIR/galdeck.sock).
    ///
    /// Point this at a development daemon running alongside the installed one.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check that the daemon is running.
    Ping,
    /// Show daemon and device state.
    Status,
    /// Set panel brightness (0-100).
    Brightness { percent: u8 },
    /// Switch to a named page in the current profile.
    Page { name: String },
    /// Switch to a named profile.
    Profile { name: String },
    /// Reload the config file and re-apply the current page.
    Reload,
    /// Print the configuration UI's address, token included.
    Ui,
    /// Enumerate the module over HID directly (works without the daemon).
    Detect,
}

fn request(request: &Request) -> Result<Response> {
    let path = galdeck_ipc::socket_path();
    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "connecting to {} — is galdeck-daemon running? (systemctl --user start galdeck)",
            path.display()
        )
    })?;
    let mut writer = stream.try_clone()?;
    let mut payload = serde_json::to_string(request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes())?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.trim().is_empty() {
        bail!("daemon closed the connection without responding");
    }
    Ok(serde_json::from_str(&line)?)
}

fn expect_ok(response: Response) -> Result<()> {
    match response {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!("{message}"),
        Response::Diagnostics { diagnostics } => {
            if diagnostics.is_empty() {
                return Ok(());
            }
            bail!("{}", render_diagnostics(&diagnostics));
        }
        other => bail!("unexpected response: {other:?}"),
    }
}

fn render_diagnostics(diagnostics: &[galdeck_ipc::Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| {
            let where_ = match d.start {
                Some(loc) => format!("{}:{}", loc.line, loc.col),
                None => d.path.clone(),
            };
            let help = d
                .help
                .as_ref()
                .map(|h| format!(" ({h})"))
                .unwrap_or_default();
            format!("{where_} [{}] {}{help}", d.code, d.message)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn detect() -> Result<()> {
    let api = galdeck::hidapi::HidApi::new()?;
    let paths = galdeck::Galleon::list(&api);
    if paths.is_empty() {
        println!("no Galleon 100 SD stream deck module found (usb 1b1c:2b18)");
        println!("- is the keyboard plugged in?");
        println!("- is the udev rule installed? (see udev/70-galdeck.rules)");
        return Ok(());
    }
    for path in paths {
        println!("module at {path}");
        // Passive open: no keepalive is sent, so the module is left in
        // whatever mode it is in — detect really is read-only.
        match galdeck::Galleon::open_passive(&api, &path) {
            Ok(mut deck) => {
                let firmware = deck.firmware_version()?;
                println!("  firmware: {firmware}");
                println!("  serial:   {}", deck.serial_number()?);
                let validated = galdeck::ids::VALIDATED_FIRMWARES;
                if !validated.contains(&firmware.as_str()) {
                    println!("  note: protocol is only validated on firmware {validated:?}");
                }
            }
            Err(e) => println!("  open failed: {e}"),
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Set before anything connects, and before any thread exists, so that
    // `socket_path()` picks it up wherever it is called from. An explicit flag
    // beats the environment, which is why this overwrites rather than checks.
    if let Some(path) = &args.socket {
        std::env::set_var("GALDECK_SOCKET", path);
    }
    match args.command {
        Command::Ping => {
            expect_ok(request(&Request::Ping)?)?;
            println!("pong");
        }
        Command::Status => match request(&Request::Status)? {
            Response::Status(status) => {
                println!("connected:  {}", status.connected);
                if let Some(firmware) = status.firmware {
                    println!("firmware:   {firmware}");
                }
                if let Some(serial) = status.serial {
                    println!("serial:     {serial}");
                }
                // Absent when talking to a daemon older than profiles.
                if !status.profile.is_empty() {
                    println!("profile:    {}", status.profile);
                }
                if !status.profiles.is_empty() {
                    println!("profiles:   {}", status.profiles.join(", "));
                }
                println!("page:       {}", status.page);
                println!("pages:      {}", status.pages.join(", "));
                println!("brightness: {}", status.brightness);
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected response: {other:?}"),
        },
        Command::Brightness { percent } => {
            expect_ok(request(&Request::SetBrightness { percent })?)?;
        }
        Command::Page { name } => {
            expect_ok(request(&Request::SwitchPage { name })?)?;
        }
        Command::Profile { name } => {
            expect_ok(request(&Request::SwitchProfile { name })?)?;
        }
        Command::Reload => {
            expect_ok(request(&Request::Reload)?)?;
        }
        Command::Ui => ui()?,
        Command::Detect => detect()?,
    }
    Ok(())
}

/// Print the address of the configuration UI.
///
/// The token lives beside the control socket, readable only by its owner, so
/// this needs no help from the daemon -- which also means it still works when
/// the browser tab holding the old one has gone stale.
fn ui() -> Result<()> {
    let socket = galdeck_ipc::socket_path();
    let token_file = socket
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/tmp"))
        .join("galdeck-ui-token");
    let token = std::fs::read_to_string(&token_file)
        .map(|t| t.trim().to_string())
        .with_context(|| {
            format!(
                "reading {} — start the daemon with --http <port> to serve the UI",
                token_file.display()
            )
        })?;
    println!("http://127.0.0.1:<port>/?token={token}");
    println!();
    println!("The port is the one passed to --http; the daemon prints the whole");
    println!("address at startup.");
    Ok(())
}
