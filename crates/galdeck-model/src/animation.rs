//! Animations.
//!
//! Every frame a key shows is a JPEG the daemon encodes, and encoding one is
//! around a millisecond of CPU. Twelve keys animating at 30 fps would be a
//! third of a core spent re-encoding pictures of the same few colours.
//!
//! So an animation declares a *small number of distinct frames*, which are
//! rendered and encoded once when the page is applied and then cycled. Playing
//! one costs a channel send and a cached `Arc<[u8]>`; the CPU cost is paid at
//! page-apply time and never again.
//!
//! Rings are different and cheaper: a ring frame is four colours, no encoding
//! at all, so those are computed per frame.

use serde::Deserialize;

use crate::color::ColorRef;

/// Slowest a full cycle may be. Beyond a minute it is not an animation.
pub const MAX_PERIOD_MS: u32 = 60_000;
/// Fastest a full cycle may be.
///
/// Below this the frames arrive faster than the panel can show them and it
/// reads as a flicker rather than motion.
pub const MIN_PERIOD_MS: u32 = 120;
/// Most frames one animation may pre-render.
///
/// Each is a full 160x160 JPEG held in memory -- around 4 KB encoded, but the
/// canvas to produce it is 76.8 KB -- so this bounds both the memory and the
/// page-apply cost.
pub const MAX_FRAMES: u8 = 32;
/// Fewest frames worth having. Two is a blink.
pub const MIN_FRAMES: u8 = 2;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnimationKind {
    /// Fade towards the target colour and back, linearly.
    Pulse,
    /// The same, eased, so it lingers at each end the way breathing does.
    Breathe,
    /// Hard alternation, no intermediate frames.
    Blink,
    /// Rings only: one lit segment travelling around.
    Spin,
    /// Rings only: a lit segment with a fading tail behind it.
    Comet,
}

impl AnimationKind {
    /// Whether this only makes sense on an encoder ring.
    pub fn is_ring_only(self) -> bool {
        matches!(self, AnimationKind::Spin | AnimationKind::Comet)
    }

    /// How far towards the target colour at this point in the cycle.
    ///
    /// `phase` runs 0..1 over one full cycle.
    pub fn mix_at(self, phase: f32) -> f32 {
        match self {
            // Up for the first half, down for the second.
            AnimationKind::Pulse => {
                if phase < 0.5 {
                    phase * 2.0
                } else {
                    (1.0 - phase) * 2.0
                }
            }
            // A raised cosine: no corner at the top or the bottom, which is
            // what makes it read as breathing rather than as a triangle wave.
            AnimationKind::Breathe => 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos(),
            AnimationKind::Blink => {
                if phase < 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            // Rings compute their own segments; this is only a fallback.
            AnimationKind::Spin | AnimationKind::Comet => 1.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Animation {
    pub kind: AnimationKind,
    /// Length of one full cycle.
    #[serde(default = "default_period_ms")]
    pub period_ms: u32,
    /// The colour it moves towards. Defaults to white, which reads as a
    /// highlight against any background.
    #[serde(default)]
    pub to: Option<ColorRef>,
    /// How many distinct frames to pre-render.
    ///
    /// More is smoother and costs more memory and more page-apply time. Eight
    /// is enough for a pulse to look continuous at these periods.
    #[serde(default = "default_frames")]
    pub frames: u8,
}

fn default_period_ms() -> u32 {
    2000
}

fn default_frames() -> u8 {
    8
}

impl Animation {
    /// Period clamped to what the hardware can actually show.
    pub fn period_ms(&self) -> u32 {
        self.period_ms.clamp(MIN_PERIOD_MS, MAX_PERIOD_MS)
    }

    /// Frame count clamped to what is worth holding in memory.
    pub fn frames(&self) -> u8 {
        match self.kind {
            // A blink has exactly two states; more would be identical copies.
            AnimationKind::Blink => 2,
            _ => self.frames.clamp(MIN_FRAMES, MAX_FRAMES),
        }
    }

    /// How long each frame is shown.
    pub fn frame_interval_ms(&self) -> u32 {
        (self.period_ms() / u32::from(self.frames())).max(1)
    }

    /// The mix for frame `index` of `frames()`.
    pub fn mix_for_frame(&self, index: u8) -> f32 {
        let phase = f32::from(index) / f32::from(self.frames());
        self.kind.mix_at(phase)
    }
}
