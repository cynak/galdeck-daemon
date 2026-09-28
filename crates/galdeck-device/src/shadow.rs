//! A software mirror of what the panel is showing.
//!
//! The daemon repaints all-or-nothing today: a page switch pushes twelve key
//! images, eight ring segments and a full 720x384 LCD frame whether or not any
//! of it changed. That is around fifty milliseconds, during which no input is
//! read.
//!
//! The fix is to remember what was written and send only the differences. The
//! mirror lives next to the code that does the writing, and is updated only
//! when a write actually succeeds — the same discipline the ring feedback
//! needed, for the same reason: claiming the hardware shows something it was
//! never told costs you every subsequent repaint of that surface.

use std::sync::Arc;

use galdeck::{Buttons, Encoders, Rgb, Ring};

use crate::DeckOp;

/// What a key should be showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyTarget {
    /// Cleared.
    Blank,
    /// A solid fill. Cheap in bytes, expensive in wall clock — it is a
    /// feature report, so 2 ms — which is why the renderer prefers a JPEG
    /// even for flat colour.
    Color(Rgb),
    /// A pre-encoded 160x160 JPEG, placed by the firmware.
    Jpeg(Arc<[u8]>),
    /// A JPEG drawn into a measured rectangle on the panel.
    ///
    /// What a calibrated deck uses: the firmware's key path blits a fixed
    /// size and cannot reach the edges of a larger key, so content that has
    /// to fill the whole keycap goes through the region path instead. The
    /// rectangle is part of the identity -- the same image at a different
    /// place is a different thing to have on screen.
    Region {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        jpeg: Arc<[u8]>,
    },
}

/// One requested change, before it has been diffed against the mirror.
///
/// The core thread speaks in these; the io thread turns them into the
/// [`DeckOp`]s that actually differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Paint {
    Brightness(u8),
    Key {
        index: u8,
        target: KeyTarget,
    },
    /// All four segments of one ring, diffed segment by segment.
    Ring {
        encoder: u8,
        colors: [Rgb; Ring::SEGMENTS as usize],
    },
    /// A full-frame LCD update.
    Lcd {
        jpeg: Arc<[u8]>,
        /// Where the frame goes. `None` is the firmware's 720x384 segment.
        /// `Some` is a calibrated screen, drawn at its measured rectangle
        /// through the panel path: the same `02 0c` command, without the
        /// segment's bounds, so a screen that shows more or less than 384
        /// rows is filled to its edges. The rectangle must be whole region
        /// blocks, and the JPEG its size.
        at: Option<galdeck::layout::Rect>,
    },
    /// Forget everything, so the next paint of each surface is unconditional.
    ///
    /// Sent when the module re-enters software mode, which wipes
    /// firmware-asserted state — the ring LEDs come back white — and after a
    /// reconnect, where the mirror describes a device that is no longer there.
    Forget,
}

/// What the panel is believed to be showing. `None` means "unknown", which
/// always forces a write.
#[derive(Debug)]
pub struct DeckShadow {
    brightness: Option<u8>,
    keys: Vec<Option<KeyTarget>>,
    rings: Vec<[Option<Rgb>; Ring::SEGMENTS as usize]>,
    /// The last full frame and where it went. The place is part of the
    /// identity, as it is for a key region.
    lcd: Option<(Option<galdeck::layout::Rect>, Arc<[u8]>)>,
}

impl Default for DeckShadow {
    fn default() -> Self {
        Self::new()
    }
}

impl DeckShadow {
    pub fn new() -> Self {
        Self {
            brightness: None,
            keys: vec![None; Buttons::COUNT as usize],
            rings: vec![[None; Ring::SEGMENTS as usize]; Encoders::COUNT as usize],
            lcd: None,
        }
    }

    /// Forget everything. The next paint of each surface will be written
    /// whether or not it matches what was last sent.
    pub fn forget_everything(&mut self) {
        *self = Self::new();
    }

    /// The ops this paint actually requires.
    ///
    /// Pure: the mirror is not updated here, because the write has not
    /// happened yet. Call [`DeckShadow::record`] once it has.
    pub fn plan(&self, paint: &Paint) -> Vec<DeckOp> {
        match paint {
            Paint::Forget => Vec::new(),
            Paint::Brightness(percent) => {
                if self.brightness == Some(*percent) {
                    Vec::new()
                } else {
                    vec![DeckOp::Brightness(*percent)]
                }
            }
            Paint::Key { index, target } => {
                let current = self.keys.get(*index as usize).and_then(|k| k.as_ref());
                if current == Some(target) {
                    return Vec::new();
                }
                vec![match target {
                    KeyTarget::Blank => DeckOp::KeyClear { key: *index },
                    KeyTarget::Color(color) => DeckOp::KeyColor {
                        key: *index,
                        color: *color,
                    },
                    KeyTarget::Jpeg(jpeg) => DeckOp::KeyJpeg {
                        key: *index,
                        jpeg: Arc::clone(jpeg),
                    },
                    KeyTarget::Region {
                        x,
                        y,
                        width,
                        height,
                        jpeg,
                    } => DeckOp::KeyRegion {
                        key: *index,
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                        jpeg: Arc::clone(jpeg),
                    },
                }]
            }
            Paint::Ring { encoder, colors } => {
                let Some(current) = self.rings.get(*encoder as usize) else {
                    return Vec::new();
                };
                // Per segment, because each one is a 2 ms feature report and a
                // turn typically moves exactly two of them.
                colors
                    .iter()
                    .enumerate()
                    .filter(|(segment, color)| current[*segment] != Some(**color))
                    .map(|(segment, color)| DeckOp::RingSegment {
                        encoder: *encoder,
                        segment: segment as u8,
                        color: *color,
                    })
                    .collect()
            }
            Paint::Lcd { jpeg, at } => {
                if self
                    .lcd
                    .as_ref()
                    .is_some_and(|(place, shown)| place == at && same_bytes(shown, jpeg))
                {
                    return Vec::new();
                }
                vec![match at {
                    None => DeckOp::LcdRegion {
                        x: 0,
                        y: 0,
                        width: galdeck::Lcd::WIDTH,
                        height: galdeck::Lcd::HEIGHT,
                        jpeg: Arc::clone(jpeg),
                    },
                    Some(rect) => DeckOp::ScreenRegion {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                        jpeg: Arc::clone(jpeg),
                    },
                }]
            }
        }
    }

    /// Record an op that was successfully written.
    pub fn record(&mut self, op: &DeckOp) {
        match op {
            DeckOp::Brightness(percent) => self.brightness = Some(*percent),
            DeckOp::KeyJpeg { key, jpeg } => {
                self.set_key(*key, KeyTarget::Jpeg(Arc::clone(jpeg)));
            }
            DeckOp::KeyColor { key, color } => self.set_key(*key, KeyTarget::Color(*color)),
            DeckOp::KeyClear { key } => self.set_key(*key, KeyTarget::Blank),
            DeckOp::KeyRegion {
                key,
                x,
                y,
                width,
                height,
                jpeg,
            } => self.set_key(
                *key,
                KeyTarget::Region {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                    jpeg: Arc::clone(jpeg),
                },
            ),
            DeckOp::RingSegment {
                encoder,
                segment,
                color,
            } => {
                if let Some(ring) = self.rings.get_mut(*encoder as usize) {
                    if let Some(slot) = ring.get_mut(*segment as usize) {
                        *slot = Some(*color);
                    }
                }
            }
            DeckOp::LcdRegion {
                x,
                y,
                width,
                height,
                jpeg,
            } => {
                // Only a full-frame write tells us the whole panel. A partial
                // region leaves the rest unknown, so the safe record is to
                // forget what the LCD shows rather than to claim this patch.
                let full = *x == 0
                    && *y == 0
                    && *width == galdeck::Lcd::WIDTH
                    && *height == galdeck::Lcd::HEIGHT;
                self.lcd = full.then(|| (None, Arc::clone(jpeg)));
            }
            DeckOp::ScreenRegion {
                x,
                y,
                width,
                height,
                jpeg,
            } => {
                let rect = galdeck::layout::Rect::new(*x, *y, *width, *height);
                self.lcd = Some((Some(rect), Arc::clone(jpeg)));
            }
            DeckOp::ClearAll => {
                self.brightness = None;
                self.keys.fill(Some(KeyTarget::Blank));
                self.rings.fill([Some(Rgb::BLACK); Ring::SEGMENTS as usize]);
                self.lcd = None;
            }
            // Leaves software-mode imagery entirely; nothing we believed still
            // holds.
            DeckOp::ResetToLogo => self.forget_everything(),
        }
    }

    fn set_key(&mut self, index: u8, target: KeyTarget) {
        if let Some(slot) = self.keys.get_mut(index as usize) {
            *slot = Some(target);
        }
    }
}

/// Cheap first, exact second: the frame cache hands back the same `Arc` for
/// unchanged content, so the pointer check carries almost every comparison and
/// a 17 KB memcmp is the rare path.
fn same_bytes(a: &Arc<[u8]>, b: &Arc<[u8]>) -> bool {
    Arc::ptr_eq(a, b) || a == b
}
