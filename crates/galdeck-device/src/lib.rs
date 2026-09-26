//! The device seam.
//!
//! The daemon talks to the deck through the [`Deck`] trait rather than to
//! `galdeck::Galleon` directly. Two things fall out of that:
//!
//! * Every drawing method takes **pre-encoded JPEG bytes**. JPEG encoding is
//!   single-threaded and runs on whichever thread calls it, so keeping it out
//!   of this trait is what lets the compositor encode on a worker while the
//!   thread that owns the HID handle does nothing but push bytes and poll.
//! * The whole daemon can run against [`FakeDeck`] with no hardware attached,
//!   which is what makes theme, widget and animation logic testable on a CI
//!   runner with no Galleon plugged in.
//!
//! Errors stay as [`galdeck::Error`] so the reconnect path can keep telling
//! `DeviceNotFound` apart from a transport failure.
//!
//! `Galleon` is named in exactly one file of this workspace, [`hardware`], and
//! a CI job enforces that.

use std::sync::Arc;
use std::time::Duration;

use galdeck::{Buttons, Encoders, Error, Event, Lcd, Rgb, Ring};

mod error;
mod fake;
mod hardware;
mod shadow;

pub use error::{DeckError, DeckResult};
pub use fake::{DeckSurface, FakeDeck, FakeDeckHandle, KeySurface, LcdPatch, Violation};
pub use hardware::HardwareDeck;
pub use shadow::{DeckShadow, KeyTarget, Paint};

/// The forced pause after every feature report.
///
/// The firmware garbles bursts of feature reports — eight rapid ring writes
/// left every segment showing the final colour — so the framework sleeps 2 ms
/// after each one. It is the dominant cost in the whole system and the unit
/// the io pump budgets in.
pub const FEATURE_REPORT_COST: Duration = Duration::from_millis(2);

/// Modelled cost of pushing one 1024-byte image report.
///
/// Image uploads are *not* paced, and at high speed the bus is never the
/// bottleneck, so this is really the cost of one `write()` syscall. Measured
/// well under this; the round number keeps the budget conservative.
pub const IMAGE_REPORT_COST: Duration = Duration::from_micros(25);

/// Modelled cost of encoding one full 720x384 LCD frame.
///
/// `image`'s JPEG encoder is single-threaded and hardwired to 4:4:4 chroma,
/// so a full frame is around ten milliseconds of CPU on the calling thread.
/// Partial regions scale with area, which is the entire reason dirty-rect
/// rendering is worth building.
pub const LCD_FULL_FRAME_ENCODE: Duration = Duration::from_millis(10);

/// Payload bytes per key-image report (1024 minus an 8-byte header).
const KEY_IMAGE_PAYLOAD: usize = 1016;
/// Payload bytes per LCD-region report (1024 minus a 16-byte header).
const LCD_REGION_PAYLOAD: usize = 1008;

/// Everything the daemon needs from a deck.
///
/// Object-safe on purpose: the io pump holds a `Box<dyn Deck>` so one code
/// path drives real hardware, the fake, and a replay.
pub trait Deck: Send {
    fn firmware(&self) -> &str;
    fn serial(&self) -> &str;

    /// Wait up to `timeout` for input. Returns as soon as any event decodes,
    /// or an empty vector when the timeout expires.
    ///
    /// Implementations must keep the keepalive alive for the whole wait:
    /// going quiet for more than two seconds makes the *next* device call
    /// block a full second while the module re-enters software mode.
    fn poll(&mut self, timeout: Duration) -> DeckResult<Vec<Event>>;

    /// True once after the module re-entered software mode, which wipes
    /// firmware-asserted state (the ring LEDs go white). The caller must
    /// forget its shadow and repaint everything when this returns true.
    ///
    /// It does *not* fire after `open()`, so the first paint of a session
    /// must be unconditional.
    fn take_mode_reentry(&mut self) -> bool;

    fn set_brightness(&mut self, percent: u8) -> DeckResult<()>;

    /// Push a pre-encoded 160x160 JPEG to a key. Keys have no partial update:
    /// this is the only way to change what one shows, short of a solid fill.
    fn set_key_jpeg(&mut self, key: u8, jpeg: &[u8]) -> DeckResult<()>;

    /// Fill a key with a solid colour. Cheap in bytes but *not* in wall clock:
    /// it is a feature report, so it costs 2 ms, which makes it roughly an
    /// order of magnitude slower than pushing a small JPEG of the same fill.
    /// Prefer the JPEG path for anything on a hot repaint route.
    fn set_key_color(&mut self, key: u8, color: Rgb) -> DeckResult<()>;

    fn clear_key(&mut self, key: u8) -> DeckResult<()>;

    /// Set one ring segment; segment 0 is the top LED, then clockwise.
    /// One feature report, so 2 ms — a full ring is 8 ms, both rings 16 ms,
    /// which is what caps ring animation at about 30 Hz once diffed.
    fn set_ring_segment(&mut self, encoder: u8, segment: u8, color: Rgb) -> DeckResult<()>;

    /// Push a pre-encoded JPEG to a rectangle of the LCD. The JPEG must decode
    /// to exactly `width` x `height` and the rectangle must fit in 720x384.
    /// Small patches are 10-50x cheaper than a full frame, so this is the
    /// primitive that dirty-rect rendering is built on.
    fn draw_lcd_jpeg(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        jpeg: &[u8],
    ) -> DeckResult<()>;

    /// Push a pre-encoded JPEG anywhere on the panel, including the key area
    /// below the info screen.
    ///
    /// The same `02 0c` report as [`Deck::draw_lcd_jpeg`], without the info
    /// screen's 720x384 contract. This is the only way to put pixels on the
    /// part of a physical key that the `02 07` key path cannot reach: that
    /// path blits a fixed-size image and does not scale it, so on a unit
    /// whose keys measure larger than [`galdeck::ids::KEY_PIXELS`] a band
    /// around every key is simply unaddressable through it.
    ///
    /// Where the keys are is not in the protocol -- it is measured per unit,
    /// which is what a calibration is -- so the caller supplies the rectangle.
    fn draw_panel_jpeg(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        jpeg: &[u8],
    ) -> DeckResult<()>;

    fn clear_all(&mut self) -> DeckResult<()>;
    fn reset_to_logo(&mut self) -> DeckResult<()>;
}

/// One device write, resolved to bytes and ready to push.
///
/// The render pool produces these; the io pump drains them against a
/// wall-clock budget. JPEG payloads are `Arc<[u8]>` so a cached sprite frame
/// is queued without copying it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeckOp {
    Brightness(u8),
    KeyJpeg {
        key: u8,
        jpeg: Arc<[u8]>,
    },
    KeyColor {
        key: u8,
        color: Rgb,
    },
    KeyClear {
        key: u8,
    },
    RingSegment {
        encoder: u8,
        segment: u8,
        color: Rgb,
    },
    LcdRegion {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        jpeg: Arc<[u8]>,
    },
    /// A key drawn as a panel region rather than through the key path.
    ///
    /// Carries the key index as well as the rectangle so the mirror can
    /// record it against that key -- a bare region tells you nothing about
    /// whose pixels it was.
    KeyRegion {
        key: u8,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        jpeg: Arc<[u8]>,
    },
    ClearAll,
    ResetToLogo,
}

impl DeckOp {
    /// What this op is expected to cost on the io thread.
    ///
    /// The io pump charges each op against a per-batch budget so a burst of
    /// ring writes cannot stall input polling. Feature reports dominate; image
    /// uploads are counted by chunk.
    pub fn cost(&self) -> Duration {
        match self {
            DeckOp::Brightness(_)
            | DeckOp::KeyColor { .. }
            | DeckOp::KeyClear { .. }
            | DeckOp::RingSegment { .. }
            | DeckOp::ResetToLogo => FEATURE_REPORT_COST,
            DeckOp::KeyJpeg { jpeg, .. } => image_cost(jpeg.len(), KEY_IMAGE_PAYLOAD),
            DeckOp::LcdRegion { jpeg, .. } | DeckOp::KeyRegion { jpeg, .. } => {
                image_cost(jpeg.len(), LCD_REGION_PAYLOAD)
            }
            // 12 key fills plus 8 ring LEDs, then a full black LCD frame.
            // 12 key fills plus 8 ring LEDs, and then -- easy to miss -- a
            // full black LCD frame, which the framework encodes on the spot
            // under its `encode` feature. That last part is most of the cost,
            // and charging only the feature reports would let the io budget
            // schedule a clear it cannot afford.
            DeckOp::ClearAll => {
                let feature_reports = u32::from(Buttons::COUNT)
                    + u32::from(Encoders::COUNT) * u32::from(Ring::SEGMENTS);
                // A flat black 720x384 frame encodes to roughly five kilobytes.
                const FLAT_LCD_FRAME_BYTES: usize = 5 * 1024;
                FEATURE_REPORT_COST * feature_reports
                    + LCD_FULL_FRAME_ENCODE
                    + image_cost(FLAT_LCD_FRAME_BYTES, LCD_REGION_PAYLOAD)
            }
        }
    }

    /// Push this op to a deck.
    pub fn apply(&self, deck: &mut dyn Deck) -> DeckResult<()> {
        match self {
            DeckOp::Brightness(percent) => deck.set_brightness(*percent),
            DeckOp::KeyJpeg { key, jpeg } => deck.set_key_jpeg(*key, jpeg),
            DeckOp::KeyColor { key, color } => deck.set_key_color(*key, *color),
            DeckOp::KeyClear { key } => deck.clear_key(*key),
            DeckOp::RingSegment {
                encoder,
                segment,
                color,
            } => deck.set_ring_segment(*encoder, *segment, *color),
            DeckOp::LcdRegion {
                x,
                y,
                width,
                height,
                jpeg,
            } => deck.draw_lcd_jpeg(*x, *y, *width, *height, jpeg),
            DeckOp::KeyRegion {
                x,
                y,
                width,
                height,
                jpeg,
                ..
            } => deck.draw_panel_jpeg(*x, *y, *width, *height, jpeg),
            DeckOp::ClearAll => deck.clear_all(),
            DeckOp::ResetToLogo => deck.reset_to_logo(),
        }
    }
}

fn image_cost(bytes: usize, payload_per_report: usize) -> Duration {
    let reports = bytes.div_ceil(payload_per_report).max(1);
    IMAGE_REPORT_COST * reports as u32
}

pub(crate) fn check_key(key: u8) -> DeckResult<()> {
    if key < Buttons::COUNT {
        Ok(())
    } else {
        Err(invalid(format!("key index {key} out of range")))
    }
}

pub(crate) fn check_ring(encoder: u8, segment: u8) -> DeckResult<()> {
    if encoder >= Encoders::COUNT {
        return Err(invalid(format!("encoder index {encoder} out of range")));
    }
    if segment >= Ring::SEGMENTS {
        return Err(invalid(format!("ring segment {segment} out of range")));
    }
    Ok(())
}

pub(crate) fn check_lcd_rect(x: u16, y: u16, width: u16, height: u16) -> DeckResult<()> {
    let fits = width > 0
        && height > 0
        && x.saturating_add(width) <= Lcd::WIDTH
        && y.saturating_add(height) <= Lcd::HEIGHT;
    if fits {
        Ok(())
    } else {
        Err(invalid(format!(
            "lcd rect {width}x{height}+{x}+{y} does not fit in {}x{}",
            Lcd::WIDTH,
            Lcd::HEIGHT
        )))
    }
}

/// The whole addressable panel, not just the info screen.
///
/// Width and height must be whole [`galdeck::ids::REGION_MCU`] blocks: the
/// firmware does not reject an off-block region, it shears it into diagonal
/// streaks, which is far harder to recognise than an error. The origin may
/// sit anywhere -- a calibrated zone's left edge routinely does not land on a
/// block boundary.
pub(crate) fn check_panel_rect(x: u16, y: u16, width: u16, height: u16) -> DeckResult<()> {
    let mcu = galdeck::ids::REGION_MCU as u16;
    if width == 0 || height == 0 || !width.is_multiple_of(mcu) || !height.is_multiple_of(mcu) {
        return Err(invalid(format!(
            "panel rect {width}x{height} must be non-zero multiples of {mcu}"
        )));
    }
    if x.saturating_add(width) > galdeck::ids::PANEL_WIDTH
        || y.saturating_add(height) > galdeck::ids::PANEL_HEIGHT
    {
        return Err(invalid(format!(
            "panel rect {width}x{height}+{x}+{y} does not fit in {}x{}",
            galdeck::ids::PANEL_WIDTH,
            galdeck::ids::PANEL_HEIGHT
        )));
    }
    Ok(())
}

pub(crate) fn invalid(message: String) -> DeckError {
    DeckError::Invalid(Error::InvalidArgument(message))
}
