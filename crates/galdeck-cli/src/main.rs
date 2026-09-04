use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use galdeck_ipc::{Request, Response};

/// Control the galdeck daemon (Corsair Galleon 100 SD Stream Deck module).
#[derive(Parser)]
#[command(version, about)]
struct Args {
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
    match Args::parse().command {
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
                println!("profile:    {}", status.profile);
                println!("profiles:   {}", status.profiles.join(", "));
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
        Command::Detect => detect()?,
    }
    Ok(())
}
