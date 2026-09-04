mod config;
mod engine;
mod ipc_server;
mod render;
mod ring;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

/// Daemon programming the Corsair Galleon 100 SD's built-in Stream Deck.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Config file (default: ~/.config/galdeck/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let config_path = args.config.unwrap_or_else(config::default_config_path);
    let config = if config_path.exists() {
        config::Config::load(&config_path)?
    } else {
        log::warn!(
            "no config at {} — running with a blank profile; copy config/galdeck.example.toml there to get started",
            config_path.display()
        );
        config::Config::fallback()
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

    let mut engine = engine::Engine::new(config_path, config, control_rx, shutdown)?;
    engine.run();

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
