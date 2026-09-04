use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;

use anyhow::Result;
use clap::ValueEnum;
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

    let (control_tx, control_rx) = channel();
    std::thread::spawn(move || ipc_server::serve(listener, control_tx));

    let mut engine = engine::Engine::new(
        config_path,
        config,
        control_rx,
        shutdown,
        args.device.into(),
    )?;
    engine.run();

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
