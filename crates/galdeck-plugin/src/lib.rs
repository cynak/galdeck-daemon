//! The galdeck plugin protocol.
//!
//! A plugin is a separate process. The daemon starts it and they exchange
//! line-delimited JSON over its stdin and stdout — one message per line, which
//! makes a plugin writable in any language and debuggable with `cat`.
//!
//! **Why stdio rather than a socket.** A socket a plugin connects to is a
//! socket anything else can connect to, and this protocol can put text and
//! colour on a key the user is about to press. Stdio has no address to find:
//! the only way to speak it is to be a process the daemon started, which makes
//! "who may talk to the daemon" a question about file permissions on the
//! manifest rather than about authenticating a connection.
//!
//! **What a plugin may do is deliberately small.** It sets text and colour on
//! keys it was given, and is told when they appear, disappear and are pressed.
//! It cannot push arbitrary images, because then the theme would stop meaning
//! anything; it cannot claim keys the config did not give it; and it cannot
//! ask the daemon to run a command, because a plugin that can do that is just
//! a worse way to write `exec`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The protocol version this build speaks.
///
/// Sent in the hello, so a plugin can refuse a daemon it does not understand
/// rather than misbehaving in some more interesting way.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest line either side will read.
///
/// A plugin is not trusted to be well-behaved, and an unbounded read on a pipe
/// it controls is a way to be talked out of all your memory.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// What the daemon says to a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToPlugin {
    /// First message. Nothing else is sent until the plugin answers `ready`.
    Hello {
        protocol: u32,
        /// The plugin's own id, so one binary can serve several manifests.
        plugin: String,
    },
    /// A key bound to this plugin is now on the visible page.
    ///
    /// Nothing is shown for a key between `disappear` and the next `appear`,
    /// so a plugin can stop working entirely while its page is not up.
    Appear {
        key: u8,
        /// Whatever the key's config passed through, verbatim.
        options: BTreeMap<String, String>,
    },
    /// That key is no longer visible.
    Disappear { key: u8 },
    /// The user pressed it.
    Press { key: u8 },
    /// Time to exit. The daemon waits briefly, then kills.
    Shutdown,
}

/// What a plugin says to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromPlugin {
    /// Answer to `hello`. Until this arrives the plugin is not asked for
    /// anything.
    Ready {
        /// Only used in logs and in the user interface.
        name: String,
    },
    /// Show this text on a key. Replaces the key's label.
    SetText { key: u8, text: String },
    /// Tint a key's background. `#rrggbb`, or a `@token` from the theme, so a
    /// plugin can stay inside the user's palette.
    SetColor { key: u8, color: String },
    /// Put a line in the daemon's log, tagged with the plugin's id.
    Log { message: String },
}

/// `plugins/<id>/plugin.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Shown in the user interface. The id is the directory name.
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The program to run, with `sh -c`, from the plugin's own directory.
    pub command: String,
}

/// Encode a message as the daemon and plugins exchange them.
pub fn encode<T: Serialize>(message: &T) -> Result<String, serde_json::Error> {
    // One line, so the reader never has to know where a message ends.
    serde_json::to_string(message).map(|json| json + "\n")
}

/// Decode one line.
pub fn decode<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim())
}

pub mod sdk;
