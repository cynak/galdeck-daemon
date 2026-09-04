//! The configuration model: profiles, pages, themes.
//!
//! Version 1 was a single flat file of pages. Version 2 splits it by who
//! authored what and by what it is about: one small global file, one file per
//! profile, one file per theme. A v1 config still loads — it is migrated in
//! memory on the way in, so nothing on disk changes until the user asks.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::animation::Animation;
use crate::theme::StyleLayer;
use crate::widget::Widget;

/// The version this build writes and understands.
pub const CURRENT_VERSION: u32 = 2;

/// `galdeck.toml` — the small file at the root.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Global {
    /// Refused rather than guessed if it is from the future: a newer daemon
    /// may have written fields this one would silently drop.
    pub version: u32,
    /// Panel brightness 0-100. One global value: the hardware has no
    /// per-key or per-surface brightness and no way to read it back.
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    /// Font used when a theme does not name one.
    #[serde(default)]
    pub font: Option<PathBuf>,
    /// Profile to start in. Defaults to the first one, alphabetically.
    #[serde(default)]
    pub profile: Option<String>,
}

impl Default for Global {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            brightness: default_brightness(),
            font: None,
            profile: None,
        }
    }
}

fn default_brightness() -> u8 {
    60
}

/// `profiles/<id>.toml`. The id is the filename stem.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Human-readable name for the user interface.
    #[serde(default)]
    pub name: Option<String>,
    /// Theme to style this profile with.
    #[serde(default)]
    pub theme: Option<String>,
    /// Page to start on. Defaults to the first one.
    #[serde(default)]
    pub home: Option<String>,
    /// Style overrides for the whole profile.
    #[serde(default)]
    pub style: StyleLayer,
    #[serde(default)]
    pub pages: Vec<Page>,
}

impl Profile {
    pub fn page(&self, id: &str) -> Option<&Page> {
        self.pages.iter().find(|page| page.id == id)
    }

    /// The page to start on: `home` if it names a real page, else the first.
    pub fn home_index(&self) -> usize {
        self.home
            .as_deref()
            .and_then(|home| self.pages.iter().position(|page| page.id == home))
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub id: String,
    #[serde(default)]
    pub lcd_text: Option<String>,
    #[serde(default)]
    pub style: StyleLayer,
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
    #[serde(default)]
    pub encoders: Vec<EncoderConfig>,
}

/// One key, numbered row-major from the top-left of the 3x4 grid.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    pub key: u8,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub icon: Option<PathBuf>,
    /// Shell command to run when pressed.
    #[serde(default)]
    pub exec: Option<String>,
    /// Page to switch to when pressed.
    #[serde(default)]
    pub page: Option<String>,
    /// Profile to switch to when pressed.
    #[serde(default)]
    pub profile: Option<String>,
    /// Return to the previous page when pressed.
    #[serde(default)]
    pub back: bool,
    #[serde(default)]
    pub style: StyleLayer,
    /// Makes this key move. Frames are pre-rendered when the page is applied.
    #[serde(default)]
    pub animation: Option<Animation>,
    /// Makes this key show something that changes.
    ///
    /// The widget's text replaces the label once it has produced one; until
    /// then, and whenever it fails, the label is what shows.
    #[serde(default)]
    pub widget: Option<Widget>,
}

impl KeyConfig {
    /// Whether pressing this key does anything at all.
    pub fn is_bound(&self) -> bool {
        self.exec.is_some() || self.page.is_some() || self.profile.is_some() || self.back
    }
}

/// One rotary encoder: 0 is the left knob, 1 the right.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncoderConfig {
    pub encoder: u8,
    #[serde(default)]
    pub press: Option<String>,
    #[serde(default)]
    pub cw: Option<String>,
    #[serde(default)]
    pub ccw: Option<String>,
    #[serde(default)]
    pub style: StyleLayer,
    /// Makes this ring move when it is at rest. Turn and click feedback still
    /// takes precedence -- an animation must not hide what the knob is doing.
    #[serde(default)]
    pub animation: Option<Animation>,
}

/// Everything loaded from a config directory.
#[derive(Clone, Debug, Default)]
pub struct Workspace {
    pub global: Global,
    pub profiles: BTreeMap<String, Profile>,
    pub themes: BTreeMap<String, crate::theme::Theme>,
}

impl Workspace {
    /// The profile to start in.
    pub fn start_profile(&self) -> Option<&str> {
        self.global
            .profile
            .as_deref()
            .filter(|id| self.profiles.contains_key(*id))
            .or_else(|| self.profiles.keys().next().map(String::as_str))
    }

    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.get(id)
    }
}
