//! Control-socket protocol between `galdeck-daemon` and `galdeck`.
//!
//! Transport: a unix stream socket, one JSON-encoded [`Request`] per line
//! from the client, answered by one JSON-encoded [`Response`] per line.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    SetBrightness {
        percent: u8,
    },
    SwitchPage {
        name: String,
    },
    /// Switch to another profile.
    SwitchProfile {
        name: String,
    },
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error { message: String },
    Status(Status),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub connected: bool,
    pub firmware: Option<String>,
    pub serial: Option<String>,
    pub page: String,
    pub pages: Vec<String>,
    pub brightness: u8,
}

/// Path of the daemon's control socket: `$XDG_RUNTIME_DIR/galdeck.sock`,
/// falling back to a per-user directory under /tmp.
///
/// The fallback keys on the numeric uid rather than `$USER`. `$USER` is
/// attacker-controlled in the general case and simply absent in some service
/// managers, where it previously collapsed to a single shared
/// `/tmp/galdeck-unknown.sock` in a world-writable directory. Two accounts
/// would then race for one path, and this socket is about to be able to
/// define what commands the daemon runs.
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("galdeck.sock");
        }
    }
    fallback_dir().join("galdeck.sock")
}

/// The `/tmp` fallback directory for this user.
pub fn fallback_dir() -> PathBuf {
    // Safety: getuid cannot fail and touches no memory.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/galdeck-{uid}"))
}
