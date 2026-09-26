use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;

use anyhow::Result;
use clap::ValueEnum;
use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::io::IoThread;
use galdeck_daemon::{engine, ipc_server};

use clap::Parser;
use engine::DeviceMode;

/// Daemon programming the Corsair Galleon 100 SD's built-in Stream Deck.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Which deck to drive: the keyboard, or one that exists only in memory
    /// so the daemon can run on a machine with no hardware attached.
    #[arg(long, value_enum, default_value_t = DeviceArg::Auto)]
    device: DeviceArg,

    /// Config directory (default: ~/.config/galdeck)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Control socket path (default: $GALDECK_SOCKET, else
    /// $XDG_RUNTIME_DIR/galdeck.sock).
    ///
    /// Give this a path of its own to run a second daemon alongside an
    /// installed one, which is what developing against `--device virtual`
    /// wants.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Draw keys through the firmware's key path only.
    ///
    /// By default a calibrated deck draws each key at the rectangle the
    /// calibration measured, so backgrounds, icons and animations fill the
    /// whole keycap instead of the smaller square the key path can reach.
    /// Pass this to go back to that square -- the escape hatch if the region
    /// path turns out not to render on some part of your panel.
    #[arg(long)]
    legacy_key_images: bool,

    /// Serve the configuration UI on loopback at this port.
    ///
    /// Off unless given, because this surface can set the shell commands the
    /// daemon runs. Pass 0 to let the system choose a free port; the address,
    /// with its token, is printed at startup.
    #[arg(long, value_name = "PORT")]
    http: Option<u16>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let config_dir = args
        .config
        .unwrap_or_else(galdeck_model::default_config_dir);
    let (workspace, diagnostics) = galdeck_model::Workspace::load(&config_dir);
    for diagnostic in &diagnostics {
        match diagnostic.severity {
            galdeck_model::Severity::Error => {
                log::error!("{}: {}", diagnostic.path, diagnostic.message)
            }
            galdeck_model::Severity::Warning => {
                log::warn!("{}: {}", diagnostic.path, diagnostic.message)
            }
            galdeck_model::Severity::Hint => {
                log::info!("{}: {}", diagnostic.path, diagnostic.message)
            }
        }
    }
    let workspace = match workspace {
        Some(workspace) => workspace,
        None => {
            log::warn!(
                "no usable config in {} — running with a blank profile; copy config/galdeck.example.toml to {}/config.toml to get started",
                config_dir.display(),
                config_dir.display()
            );
            galdeck_model::Workspace::default()
        }
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::Relaxed);
        })?;
    }

    let socket_path = args.socket.clone().unwrap_or_else(galdeck_ipc::socket_path);
    let listener = ipc_server::bind(&socket_path)?;
    log::info!("control socket: {}", socket_path.display());

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let deadline = Arc::new(DeadlineCell::new());
    let (waker, wake_rx) = wake_channel();

    let (control_tx, control_rx) = channel();
    let control = engine::ControlSender::new(control_tx, waker.clone());
    {
        let control = control.clone();
        std::thread::spawn(move || ipc_server::serve(listener, control));
    }

    let preview = galdeck_daemon::preview::Preview::new();
    if let Some(port) = args.http {
        // Derived from the socket actually in use rather than from the
        // environment, or `--socket` would move the socket and leave the
        // token behind somewhere `galdeck ui` cannot find it.
        let token_file = galdeck_daemon::http::token_path_for(&socket_path);
        let server = galdeck_daemon::http::HttpServer::bind_with_token(
            port,
            control.clone(),
            preview.clone(),
            Arc::clone(&shutdown),
            token_file,
        )?;
        println!("configuration UI: {}", server.url());
        std::thread::Builder::new()
            .name("galdeck-ui".into())
            .spawn(move || server.run())?;
    }

    // A whole page is sixteen paints. Sixty-four leaves room for a page
    // switch landing on top of a half-drained one; beyond that the device is
    // further behind than a page, which only happens when it has stopped
    // answering, and the io thread is about to reconnect and repaint anyway.
    let (paint_tx, paint_rx) = sync_channel(64);
    let (device_tx, device_rx) = sync_channel(256);

    let (widget_host, widget_rx) = galdeck_daemon::widgets::WidgetHost::new(waker.clone());
    let (plugin_host, plugin_rx) =
        galdeck_daemon::plugins::PluginHost::discover(&config_dir, waker.clone());

    // Shared with the engine, which sets it to hand the device to another
    // process -- `galdeck calibrate` is the one that does.
    let parked = Arc::new(AtomicBool::new(false));

    let io = IoThread::new(
        args.device.into(),
        paint_rx,
        device_tx,
        Arc::clone(&deadline),
        Arc::clone(&clock),
        waker,
        Arc::clone(&shutdown),
        Arc::clone(&parked),
    );
    let io_thread = std::thread::Builder::new()
        .name("galdeck-io".into())
        .spawn(move || io.run())?;

    let mut engine = engine::Engine::new(
        config_dir,
        workspace,
        engine::EngineParts {
            control_rx,
            device_rx,
            paint_tx,
            widget_rx,
            wake: wake_rx,
            deadline,
            clock,
            shutdown,
            preview,
            widget_host,
            plugin_host,
            plugin_rx,
            parked,
            zone_paint: !args.legacy_key_images,
            calibration_path: None,
        },
    )?;
    engine.run();

    // The io thread owns the device and blanks it on the way out; wait for
    // that rather than leaving the panel mid-page.
    let _ = io_thread.join();
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum DeviceArg {
    /// Use the keyboard if it is present, and keep retrying if it is not.
    Auto,
    /// A deck that exists only in memory.
    Virtual,
}

impl From<DeviceArg> for DeviceMode {
    fn from(arg: DeviceArg) -> Self {
        match arg {
            DeviceArg::Auto => DeviceMode::Auto,
            DeviceArg::Virtual => DeviceMode::Virtual,
        }
    }
}
