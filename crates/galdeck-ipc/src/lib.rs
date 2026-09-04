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
    SetBrightness { percent: u8 },
    SwitchPage { name: String },
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
/// falling back to a per-user name under /tmp.
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("galdeck.sock");
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    PathBuf::from(format!("/tmp/galdeck-{user}.sock"))
}
