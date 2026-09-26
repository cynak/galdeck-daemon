//! A deck that exists only in memory.
//!
//! It records what it was told to draw, in order, and hands out injected
//! input — so the engine, compositor, widgets, animations and the web UI can
//! all be driven end to end on a machine with no keyboard attached. This is
//! what makes `--device virtual` and the golden tests possible.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use galdeck::{Buttons, Encoders, Event, Lcd, Rgb, Ring};
use galdeck_core::{Clock, ManualClock, Tick};

use crate::{
    check_key, check_lcd_rect, check_panel_rect, check_ring, invalid, Deck, DeckOp, DeckResult,
};

/// How long the module may go untouched before the firmware drops out of
/// software mode. Mirrors `galdeck::ids::SOFTWARE_MODE_REENTRY_GAP`.
const REENTRY_GAP: Duration = Duration::from_secs(2);
/// What the next call costs once that has happened: the framework sleeps a
/// full second waiting for the firmware to finish asserting its own state.
const REENTRY_SETTLE: Duration = Duration::from_millis(1000);

/// Something the real firmware would not forgive.
///
/// These are recorded rather than returned, because the hardware does not
/// return them either -- it misbehaves quietly. Surfacing them here is the
/// main reason the fake is worth more than a stub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// A JPEG that does not decode to the rectangle it was sent for.
    ///
    /// `protocol::lcd_region_reports` documents that the JPEG "must decode to
    /// exactly width x height" and then never checks it, and
    /// `key_image_reports` never checks for 160x160 either. The firmware's
    /// behaviour when they disagree is undefined.
    JpegDimensionMismatch {
        what: String,
        expected: (u32, u32),
        actual: (u32, u32),
    },
    /// A payload that is not a decodable JPEG at all.
    UndecodableJpeg { what: String },
    /// The module went untouched for long enough to drop out of software
    /// mode. On real hardware the next call then blocks for a second and the
    /// ring LEDs come back white.
    KeepaliveGap { gap: Duration },
}

/// What one key is currently showing.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum KeySurface {
    /// Cleared — the module shows nothing for this key.
    #[default]
    Blank,
    Color(Rgb),
    Jpeg(Arc<[u8]>),
}

/// One rectangular JPEG write to the LCD, in the order it was issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LcdPatch {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub jpeg: Arc<[u8]>,
}

impl LcdPatch {
    /// Whether this patch covers the whole drawable segment, which makes
    /// every earlier patch invisible.
    pub fn is_full_frame(&self) -> bool {
        self.x == 0 && self.y == 0 && self.width == Lcd::WIDTH && self.height == Lcd::HEIGHT
    }
}

/// Everything a [`FakeDeck`] has been told to display, plus counters for the
/// two write classes whose costs differ by an order of magnitude.
#[derive(Clone, Debug)]
pub struct DeckSurface {
    pub brightness: u8,
    pub keys: Vec<KeySurface>,
    pub rings: Vec<[Rgb; Ring::SEGMENTS as usize]>,
    /// LCD writes since the last full-frame cover, oldest first.
    pub lcd: Vec<LcdPatch>,
    /// Panel-region writes, oldest first. Kept apart from `lcd` because they
    /// address the key area, which no full-frame info-screen write covers.
    pub panel: Vec<LcdPatch>,
    /// Every op applied, in order. This is the stream the P0 refactor proof
    /// compares byte for byte.
    pub ops: Vec<DeckOp>,
    /// Feature reports issued. Each costs a forced 2 ms sleep on real
    /// hardware, so this is the number dirty-tracking tests assert on.
    pub feature_writes: u64,
    /// Image uploads issued (key JPEGs and LCD regions). Unpaced.
    pub image_writes: u64,
    /// Bumped on every change, so a watcher can tell whether anything moved.
    pub generation: u64,
    /// Things the real firmware would not forgive, in the order they happened.
    pub violations: Vec<Violation>,
}

impl Default for DeckSurface {
    fn default() -> Self {
        Self {
            brightness: 0,
            keys: vec![KeySurface::Blank; Buttons::COUNT as usize],
            rings: vec![[Rgb::BLACK; Ring::SEGMENTS as usize]; Encoders::COUNT as usize],
            lcd: Vec::new(),
            panel: Vec::new(),
            ops: Vec::new(),
            feature_writes: 0,
            image_writes: 0,
            generation: 0,
            violations: Vec::new(),
        }
    }
}

impl DeckSurface {
    pub fn key(&self, index: u8) -> &KeySurface {
        &self.keys[index as usize]
    }

    pub fn ring(&self, encoder: u8) -> &[Rgb; Ring::SEGMENTS as usize] {
        &self.rings[encoder as usize]
    }

    /// Total device writes, both classes.
    pub fn writes(&self) -> u64 {
        self.feature_writes + self.image_writes
    }

    /// Modelled wall-clock cost of everything written so far.
    pub fn cost(&self) -> Duration {
        self.ops.iter().map(DeckOp::cost).sum()
    }
}

pub struct FakeDeck {
    surface: Arc<Mutex<DeckSurface>>,
    input: Receiver<Event>,
    reentry: Arc<Mutex<bool>>,
    firmware: String,
    serial: String,
    /// When set, time is simulated: polls and writes advance this clock
    /// instead of blocking, so a sixty-second soak runs in microseconds.
    clock: Option<Arc<ManualClock>>,
    /// Simulated time of the last device interaction, for gap detection.
    last_touch: Tick,
}

/// The test/preview side of a [`FakeDeck`]: injects input, reads the surface.
#[derive(Clone)]
pub struct FakeDeckHandle {
    surface: Arc<Mutex<DeckSurface>>,
    input: Sender<Event>,
    reentry: Arc<Mutex<bool>>,
}

impl FakeDeck {
    /// A fake deck that runs in real time, for `--device virtual`.
    pub fn new() -> (Self, FakeDeckHandle) {
        Self::build("3.05.003", "FAKE0000000000", None)
    }

    /// A fake deck whose time is simulated.
    ///
    /// Polls advance the clock by their timeout instead of blocking, and each
    /// write charges its modelled cost, so an hour-long soak runs instantly
    /// and a keepalive bug shows up as a gap rather than as a flaky timing
    /// assertion.
    pub fn simulated(clock: Arc<ManualClock>) -> (Self, FakeDeckHandle) {
        Self::build("3.05.003", "FAKE0000000000", Some(clock))
    }

    pub fn with_identity(firmware: &str, serial: &str) -> (Self, FakeDeckHandle) {
        Self::build(firmware, serial, None)
    }

    fn build(
        firmware: &str,
        serial: &str,
        clock: Option<Arc<ManualClock>>,
    ) -> (Self, FakeDeckHandle) {
        let (tx, rx) = std::sync::mpsc::channel();
        let surface = Arc::new(Mutex::new(DeckSurface::default()));
        let reentry = Arc::new(Mutex::new(false));
        let last_touch = clock.as_ref().map(|c| c.now()).unwrap_or(Tick::ZERO);
        let deck = Self {
            surface: Arc::clone(&surface),
            input: rx,
            reentry: Arc::clone(&reentry),
            firmware: firmware.to_string(),
            serial: serial.to_string(),
            clock,
            last_touch,
        };
        let handle = FakeDeckHandle {
            surface,
            input: tx,
            reentry,
        };
        (deck, handle)
    }

    /// Note that the device was touched, and replicate what the firmware does
    /// when it has not been touched for too long.
    fn touch(&mut self) {
        let Some(clock) = self.clock.as_ref() else {
            return;
        };
        let now = clock.now();
        let gap = now.duration_since(self.last_touch);
        if gap >= REENTRY_GAP {
            self.note(Violation::KeepaliveGap { gap });
            // The framework sleeps a full second here, and the firmware wipes
            // the ring LEDs on the way back in.
            clock.advance(REENTRY_SETTLE);
            *self.reentry.lock().expect("reentry flag poisoned") = true;
        }
        self.last_touch = clock.now();
    }

    fn note(&self, violation: Violation) {
        let mut surface = self.surface.lock().expect("deck surface poisoned");
        surface.violations.push(violation);
    }

    /// Check a payload really is a JPEG of the size it was sent for.
    fn check_jpeg(&self, what: &str, jpeg: &[u8], expected: (u32, u32)) {
        match jpeg_dimensions(jpeg) {
            Some(actual) if actual == expected => {}
            Some(actual) => self.note(Violation::JpegDimensionMismatch {
                what: what.to_string(),
                expected,
                actual,
            }),
            None => self.note(Violation::UndecodableJpeg {
                what: what.to_string(),
            }),
        }
    }

    fn record<F: FnOnce(&mut DeckSurface)>(&mut self, op: DeckOp, f: F) {
        self.touch();
        if let Some(clock) = self.clock.as_ref() {
            clock.advance(op.cost());
        }
        let mut surface = self.surface.lock().expect("deck surface poisoned");
        f(&mut surface);
        surface.ops.push(op);
        surface.generation += 1;
    }
}

/// Dimensions of a JPEG without decoding its pixels.
fn jpeg_dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::with_format(std::io::Cursor::new(jpeg), image::ImageFormat::Jpeg)
        .into_dimensions()
        .ok()
}

impl FakeDeckHandle {
    /// Queue an input event for the next `poll`.
    pub fn press(&self, event: Event) {
        // A closed receiver just means the deck has been dropped; a test that
        // does this is asserting on the surface, not on delivery.
        let _ = self.input.send(event);
    }

    /// Snapshot of what the deck is showing.
    pub fn surface(&self) -> DeckSurface {
        self.surface.lock().expect("deck surface poisoned").clone()
    }

    /// The op stream so far.
    pub fn ops(&self) -> Vec<DeckOp> {
        self.surface
            .lock()
            .expect("deck surface poisoned")
            .ops
            .clone()
    }

    /// Everything the real firmware would not have forgiven.
    pub fn violations(&self) -> Vec<Violation> {
        self.surface
            .lock()
            .expect("deck surface poisoned")
            .violations
            .clone()
    }

    /// Forget the op stream, keeping the displayed surface. Useful for
    /// asserting what a *subsequent* action wrote.
    pub fn clear_ops(&self) {
        self.surface
            .lock()
            .expect("deck surface poisoned")
            .ops
            .clear();
    }

    /// Make the next `take_mode_reentry` return true, as the firmware does
    /// when it re-enters software mode and wipes the ring LEDs.
    pub fn signal_mode_reentry(&self) {
        *self.reentry.lock().expect("reentry flag poisoned") = true;
    }
}

impl Deck for FakeDeck {
    fn firmware(&self) -> &str {
        &self.firmware
    }

    fn serial(&self) -> &str {
        &self.serial
    }

    fn poll(&mut self, timeout: Duration) -> DeckResult<Vec<Event>> {
        self.touch();
        let mut events = Vec::new();

        if let Some(clock) = self.clock.as_ref() {
            // Simulated: never block. Take whatever is queued, and if there is
            // nothing, let the timeout elapse in simulated time.
            while let Ok(event) = self.input.try_recv() {
                events.push(event);
            }
            if events.is_empty() {
                clock.advance(timeout);
            }
            self.last_touch = clock.now();
            return Ok(events);
        }

        match self.input.recv_timeout(timeout) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Timeout) => return Ok(events),
            // The handle is gone. Report no input rather than an error: a
            // dropped handle is a finished test, not a broken device.
            Err(RecvTimeoutError::Disconnected) => return Ok(events),
        }
        // Drain whatever else is already queued, the way one input report can
        // decode to several events.
        while let Ok(event) = self.input.try_recv() {
            events.push(event);
        }
        Ok(events)
    }

    fn take_mode_reentry(&mut self) -> bool {
        let mut flag = self.reentry.lock().expect("reentry flag poisoned");
        std::mem::take(&mut *flag)
    }

    fn set_brightness(&mut self, percent: u8) -> DeckResult<()> {
        if percent > 100 {
            return Err(invalid(format!("brightness {percent} out of range")));
        }
        self.record(DeckOp::Brightness(percent), |s| {
            s.brightness = percent;
            s.feature_writes += 1;
        });
        Ok(())
    }

    fn set_key_jpeg(&mut self, key: u8, jpeg: &[u8]) -> DeckResult<()> {
        check_key(key)?;
        self.check_jpeg(
            &format!("key {key}"),
            jpeg,
            (Buttons::PIXEL_SIZE, Buttons::PIXEL_SIZE),
        );
        let jpeg: Arc<[u8]> = Arc::from(jpeg);
        self.record(
            DeckOp::KeyJpeg {
                key,
                jpeg: Arc::clone(&jpeg),
            },
            |s| {
                s.keys[key as usize] = KeySurface::Jpeg(jpeg);
                s.image_writes += 1;
            },
        );
        Ok(())
    }

    fn set_key_color(&mut self, key: u8, color: Rgb) -> DeckResult<()> {
        check_key(key)?;
        self.record(DeckOp::KeyColor { key, color }, |s| {
            s.keys[key as usize] = KeySurface::Color(color);
            s.feature_writes += 1;
        });
        Ok(())
    }

    fn clear_key(&mut self, key: u8) -> DeckResult<()> {
        check_key(key)?;
        self.record(DeckOp::KeyClear { key }, |s| {
            s.keys[key as usize] = KeySurface::Blank;
            s.feature_writes += 1;
        });
        Ok(())
    }

    fn set_ring_segment(&mut self, encoder: u8, segment: u8, color: Rgb) -> DeckResult<()> {
        check_ring(encoder, segment)?;
        self.record(
            DeckOp::RingSegment {
                encoder,
                segment,
                color,
            },
            |s| {
                s.rings[encoder as usize][segment as usize] = color;
                s.feature_writes += 1;
            },
        );
        Ok(())
    }

    fn draw_lcd_jpeg(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        jpeg: &[u8],
    ) -> DeckResult<()> {
        check_lcd_rect(x, y, width, height)?;
        self.check_jpeg(
            &format!("lcd {width}x{height}+{x}+{y}"),
            jpeg,
            (u32::from(width), u32::from(height)),
        );
        let jpeg: Arc<[u8]> = Arc::from(jpeg);
        let patch = LcdPatch {
            x,
            y,
            width,
            height,
            jpeg: Arc::clone(&jpeg),
        };
        self.record(
            DeckOp::LcdRegion {
                x,
                y,
                width,
                height,
                jpeg,
            },
            |s| {
                // A full-frame write hides everything under it, so drop the
                // history rather than growing it without bound.
                if patch.is_full_frame() {
                    s.lcd.clear();
                }
                s.lcd.push(patch);
                s.image_writes += 1;
            },
        );
        Ok(())
    }

    fn draw_panel_jpeg(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        jpeg: &[u8],
    ) -> DeckResult<()> {
        check_panel_rect(x, y, width, height)?;
        self.check_jpeg(
            &format!("panel {width}x{height}+{x}+{y}"),
            jpeg,
            (u32::from(width), u32::from(height)),
        );
        let jpeg: Arc<[u8]> = Arc::from(jpeg);
        let patch = LcdPatch {
            x,
            y,
            width,
            height,
            jpeg: Arc::clone(&jpeg),
        };
        // The key index is not carried here: this is the device primitive,
        // and the firmware has no idea a key is what it is drawing.
        self.record(
            DeckOp::KeyRegion {
                key: u8::MAX,
                x,
                y,
                width,
                height,
                jpeg,
            },
            |s| {
                s.panel.push(patch);
                s.image_writes += 1;
            },
        );
        Ok(())
    }

    fn clear_all(&mut self) -> DeckResult<()> {
        self.record(DeckOp::ClearAll, |s| {
            s.keys.fill(KeySurface::Blank);
            s.rings.fill([Rgb::BLACK; Ring::SEGMENTS as usize]);
            s.lcd.clear();
            // 12 key fills plus 8 ring LEDs, as the framework does it.
            s.feature_writes +=
                u64::from(Buttons::COUNT) + u64::from(Encoders::COUNT) * u64::from(Ring::SEGMENTS);
            s.image_writes += 1;
        });
        Ok(())
    }

    fn reset_to_logo(&mut self) -> DeckResult<()> {
        self.record(DeckOp::ResetToLogo, |s| s.feature_writes += 1);
        Ok(())
    }
}
