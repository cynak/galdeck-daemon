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

    /// Config file (default: ~/.config/galdeck/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let config_path = args
        .config
        .unwrap_or_else(galdeck_model::default_config_path);
    let config = if config_path.exists() {
        galdeck_model::Config::load(&config_path)?
    } else {
        log::warn!(
            "no config at {} — running with a blank profile; copy config/galdeck.example.toml there to get started",
            config_path.display()
        );
        galdeck_model::Config::fallback()
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::Relaxed);
        })?;
    }

    let socket_path = galdeck_ipc::socket_path();
    let listener = ipc_server::bind(&socket_path)?;
    log::info!("control socket: {}", socket_path.display());

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let deadline = Arc::new(DeadlineCell::new());
    let (waker, wake_rx) = wake_channel();

    let (control_tx, control_rx) = channel();
    let control = engine::ControlSender::new(control_tx, waker.clone());
    std::thread::spawn(move || ipc_server::serve(listener, control));

    // A whole page is sixteen paints. Sixty-four leaves room for a page
    // switch landing on top of a half-drained one; beyond that the device is
    // further behind than a page, which only happens when it has stopped
    // answering, and the io thread is about to reconnect and repaint anyway.
    let (paint_tx, paint_rx) = sync_channel(64);
    let (device_tx, device_rx) = sync_channel(256);

    let io = IoThread::new(
        args.device.into(),
        paint_rx,
        device_tx,
        Arc::clone(&deadline),
        Arc::clone(&clock),
        waker,
        Arc::clone(&shutdown),
    );
    let io_thread = std::thread::Builder::new()
        .name("galdeck-io".into())
        .spawn(move || io.run())?;

    let mut engine = engine::Engine::new(
        config_path,
        config,
        control_rx,
        device_rx,
        paint_tx,
        deadline,
        clock,
        wake_rx,
        shutdown,
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
