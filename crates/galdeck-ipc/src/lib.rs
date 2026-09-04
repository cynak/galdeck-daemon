//! The control protocol shared by the daemon, the CLI and the web UI.
//!
//! Line-delimited JSON over a Unix stream socket: one request per line, one
//! response per line, in order. Simple enough that `socat` is a usable client,
//! which matters for a protocol people will script against.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use galdeck_model::{Diagnostic, Patch, Severity, Value};

/// A request to the daemon.
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
    /// Every configuration file, as text, for an editor to work on.
    GetConfig,
    /// The current page, resolved, with the config path of every control.
    ///
    /// An editor needs to know that the key in the top-left corner is
    /// `pages[0].keys[2]` in `profiles/work.toml`. Answering that here keeps
    /// the model in one place instead of reimplemented in JavaScript.
    GetLayout,
    /// Try edits without saving anything, and report what they would do.
    ///
    /// This is what makes live validation possible: the editor can show
    /// problems in an edit nobody has committed.
    ValidateConfig {
        file: String,
        patches: Vec<Patch>,
        /// The generation the editor read. Omitted skips the check.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Apply edits, save them, and reload.
    ApplyConfig {
        file: String,
        patches: Vec<Patch>,
        #[serde(default)]
        generation: Option<u64>,
    },
}

/// A reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error {
        message: String,
    },
    Status(Status),
    Config(ConfigSnapshot),
    Layout(Layout),
    /// Everything an edit would produce. An empty list means it is clean.
    Diagnostics {
        diagnostics: Vec<Diagnostic>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub connected: bool,
    pub firmware: Option<String>,
    pub serial: Option<String>,
    /// Which profile is showing.
    pub profile: String,
    pub profiles: Vec<String>,
    pub page: String,
    pub pages: Vec<String>,
    pub brightness: u8,
}

/// The whole configuration, as the files it is written in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub dir: PathBuf,
    pub files: Vec<ConfigFile>,
    /// Everything currently wrong with it.
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    /// Path relative to the config directory, e.g. `profiles/work.toml`.
    pub name: String,
    pub text: String,
    /// Bumped on every save. An edit carrying a stale one is refused.
    pub generation: u64,
}

/// The current page, with everything an editor needs to address it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub profile: String,
    /// The file the current profile is written in, for patching.
    pub file: String,
    pub page: String,
    /// Index of the current page within that file's `pages` array.
    pub page_index: usize,
    pub pages: Vec<String>,
    pub keys: Vec<KeyInfo>,
    pub encoders: Vec<EncoderInfo>,
    /// Whether a `back` key would go anywhere.
    pub can_go_back: bool,
}

/// One configured key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyInfo {
    /// Position on the 3x4 grid, 0-11.
    pub key: u8,
    /// Index within the page's `keys` array, for building a patch path.
    pub index: usize,
    /// The label as configured, which is what an editor edits.
    pub label: Option<String>,
    /// What the key is showing right now. Differs from `label` when a widget
    /// has produced text, so an editor can show both without guessing.
    pub text: Option<String>,
    /// The widget's kind, if it has one.
    pub widget: Option<String>,
    pub icon: Option<String>,
    pub exec: Option<String>,
    pub page: Option<String>,
    pub profile: Option<String>,
    pub back: bool,
    /// The background this key resolves to, after the whole cascade.
    pub background: String,
    /// Whether the background came from this key rather than a theme above it.
    pub background_is_own: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderInfo {
    pub encoder: u8,
    pub index: usize,
    pub press: Option<String>,
    pub cw: Option<String>,
    pub ccw: Option<String>,
    pub ring: String,
    pub ring_is_own: bool,
}

/// Something that happened, for clients that asked to be told.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    DeviceConnected { firmware: String, serial: String },
    DeviceDisconnected,
    PageChanged { profile: String, page: String },
    ProfileChanged { profile: String },
    BrightnessChanged { percent: u8 },
    KeyPressed { key: u8 },
    EncoderTurned { encoder: u8, delta: i8 },
    EncoderPressed { encoder: u8 },
    ConfigChanged,
}

/// Path of the daemon's control socket: `$XDG_RUNTIME_DIR/galdeck.sock`,
/// falling back to a per-user directory under /tmp.
///
/// The fallback keys on the numeric uid rather than `$USER`. `$USER` is
/// attacker-controlled in the general case and simply absent in some service
/// managers, where it previously collapsed to a single shared
/// `/tmp/galdeck-unknown.sock` in a world-writable directory. Two accounts
/// would then race for one path, and this socket can define what commands the
/// daemon runs.
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
