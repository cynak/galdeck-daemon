//! Widgets: keys that show something that changes.
//!
//! A widget produces a line of text on a schedule. That is deliberately the
//! whole contract — a key is 160x160 pixels with room for a few characters,
//! and everything about how those characters look already belongs to the theme
//! cascade. Making widgets produce styled output would be a second styling
//! system that disagrees with the first.
//!
//! Cheap widgets are sampled on the thread that owns the daemon's state.
//! Anything that could block — running a shell command — is not, and the
//! distinction is [`WidgetKind::is_blocking`], because getting it wrong stalls
//! every key on the deck.

use serde::Deserialize;

/// Fastest a widget may refresh.
///
/// A key repaint is a JPEG encode; ten a second, per key, is already more than
/// anything worth displaying needs.
pub const MIN_INTERVAL_MS: u32 = 100;
/// Slowest before it is not really a widget.
pub const MAX_INTERVAL_MS: u32 = 24 * 60 * 60 * 1000;
/// How much of a command's output is kept.
///
/// A key shows a handful of characters, and a command that prints a megabyte
/// should not cost a megabyte of memory per refresh.
pub const MAX_OUTPUT_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    /// The time of day.
    Clock,
    /// The date.
    Date,
    /// Total CPU use across all cores, as a percentage.
    Cpu,
    /// Memory in use, as a percentage.
    Memory,
    /// The first line of a shell command's output.
    Command,
}

impl WidgetKind {
    /// Whether sampling this could block.
    ///
    /// Only `Command` can: the rest are a clock read or a few bytes from
    /// `/proc`. A blocking sample runs on a worker, because doing it on the
    /// thread that owns the deck's state would stall every other key while
    /// some script decides what to print.
    pub fn is_blocking(self) -> bool {
        matches!(self, WidgetKind::Command)
    }

    /// A sensible refresh rate for this kind.
    pub fn default_interval_ms(self) -> u32 {
        match self {
            // A minute would drift visibly against the wall clock.
            WidgetKind::Clock => 1_000,
            WidgetKind::Date => 60_000,
            WidgetKind::Cpu | WidgetKind::Memory => 2_000,
            // Someone else's script; do not run it more than necessary.
            WidgetKind::Command => 5_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Widget {
    pub kind: WidgetKind,
    /// How often to refresh. Defaults to something sensible per kind.
    #[serde(default)]
    pub interval_ms: Option<u32>,
    /// For `clock` and `date`: a strftime-style format.
    #[serde(default)]
    pub format: Option<String>,
    /// For `command`: what to run, with `sh -c`.
    #[serde(default)]
    pub command: Option<String>,
    /// Text shown before the widget has produced anything, and whenever it
    /// fails. Falls back to the key's label.
    #[serde(default)]
    pub placeholder: Option<String>,
}

impl Widget {
    pub fn interval_ms(&self) -> u32 {
        self.interval_ms
            .unwrap_or_else(|| self.kind.default_interval_ms())
            .clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
    }

    /// The format string to use, with a default per kind.
    pub fn format(&self) -> &str {
        self.format.as_deref().unwrap_or(match self.kind {
            WidgetKind::Date => "%a %d %b",
            _ => "%H:%M",
        })
    }
}
