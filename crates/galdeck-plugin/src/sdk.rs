//! Enough to write a plugin in Rust without thinking about the wire.
//!
//! A plugin is a loop: read a message, do something, maybe say something back.
//! This is that loop, with the framing and the flushing handled — the flushing
//! being the part that is easy to forget and produces a plugin that appears to
//! hang while its output sits in a buffer.

use std::io::{BufRead, BufReader, Write};

use crate::{decode, encode, FromPlugin, ToPlugin, MAX_LINE_BYTES};

/// The plugin side of the connection.
pub struct Plugin {
    stdout: std::io::Stdout,
}

impl Default for Plugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin {
    pub fn new() -> Self {
        Self {
            stdout: std::io::stdout(),
        }
    }

    /// Say something to the daemon.
    pub fn send(&mut self, message: &FromPlugin) -> std::io::Result<()> {
        let line = encode(message).map_err(std::io::Error::other)?;
        self.stdout.write_all(line.as_bytes())?;
        // Without this the daemon sees nothing until the buffer happens to
        // fill, which looks exactly like a plugin that has stopped working.
        self.stdout.flush()
    }

    pub fn set_text(&mut self, key: u8, text: impl Into<String>) -> std::io::Result<()> {
        self.send(&FromPlugin::SetText {
            key,
            text: text.into(),
        })
    }

    pub fn set_color(&mut self, key: u8, color: impl Into<String>) -> std::io::Result<()> {
        self.send(&FromPlugin::SetColor {
            key,
            color: color.into(),
        })
    }

    pub fn log(&mut self, message: impl Into<String>) -> std::io::Result<()> {
        self.send(&FromPlugin::Log {
            message: message.into(),
        })
    }
}

/// Run a plugin.
///
/// Answers the hello, then calls `handler` for every message. Returns when the
/// daemon says to shut down or closes the pipe.
pub fn run<F>(name: &str, mut handler: F) -> std::io::Result<()>
where
    F: FnMut(&mut Plugin, ToPlugin) -> std::io::Result<()>,
{
    let mut plugin = Plugin::new();
    let stdin = BufReader::new(std::io::stdin());

    for line in stdin.lines() {
        let line = line?;
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(message) = decode::<ToPlugin>(&line) else {
            // An unknown message is ignored rather than fatal, so a daemon
            // that grows a new one does not break every existing plugin.
            continue;
        };
        match message {
            ToPlugin::Hello { .. } => {
                plugin.send(&FromPlugin::Ready {
                    name: name.to_string(),
                })?;
            }
            ToPlugin::Shutdown => return Ok(()),
            other => handler(&mut plugin, other)?,
        }
    }
    Ok(())
}
