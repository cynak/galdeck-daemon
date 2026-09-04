//! The real device.
//!
//! This is the only file in the workspace that names `Galleon`; a CI job
//! enforces that, which is what keeps every other crate testable without
//! hardware.

use std::time::Duration;

use galdeck::{Error, Event, Galleon, Rgb};

use crate::Deck;

pub struct HardwareDeck {
    deck: Galleon,
    firmware: String,
    serial: String,
}

impl HardwareDeck {
    /// Open the first Galleon the system reports.
    ///
    /// This blocks for about 1.2 seconds: the framework waits 200 ms after
    /// opening before any traffic, then sends the keepalive that enters
    /// software mode, then waits a further second for the firmware to finish
    /// asserting its own state. That is why opening happens on the io thread
    /// and never on the thread that answers control requests.
    pub fn open(api: &galdeck::hidapi::HidApi) -> Result<Self, Error> {
        let mut deck = Galleon::open(api)?;
        // Identity is read once and cached: both getters are feature reports,
        // and nothing about them changes while the handle is open.
        let firmware = deck
            .firmware_version()
            .unwrap_or_else(|_| "unknown".to_string());
        let serial = deck
            .serial_number()
            .unwrap_or_else(|_| "unknown".to_string());
        Ok(Self {
            deck,
            firmware,
            serial,
        })
    }

    /// Whether this firmware is one the protocol has been validated against.
    ///
    /// Firmware 3.06.006 and later reportedly change the keepalive report, and
    /// nothing public implements that, so an unknown version is worth warning
    /// about rather than failing on.
    pub fn firmware_is_validated(&self) -> bool {
        galdeck::ids::VALIDATED_FIRMWARES.contains(&self.firmware.as_str())
    }
}

impl Deck for HardwareDeck {
    fn firmware(&self) -> &str {
        &self.firmware
    }

    fn serial(&self) -> &str {
        &self.serial
    }

    fn poll(&mut self, timeout: Duration) -> Result<Vec<Event>, Error> {
        // Galleon::poll slices internally at 100 ms and ticks the keepalive at
        // the top of each slice, so a long timeout here is still safe.
        self.deck.poll(timeout)
    }

    fn take_mode_reentry(&mut self) -> bool {
        self.deck.take_mode_reentry()
    }

    fn set_brightness(&mut self, percent: u8) -> Result<(), Error> {
        self.deck.set_brightness(percent)
    }

    fn set_key_jpeg(&mut self, key: u8, jpeg: &[u8]) -> Result<(), Error> {
        self.deck.button(key)?.set_jpeg(jpeg)
    }

    fn set_key_color(&mut self, key: u8, color: Rgb) -> Result<(), Error> {
        self.deck.button(key)?.set_color(color)
    }

    fn clear_key(&mut self, key: u8) -> Result<(), Error> {
        self.deck.button(key)?.clear()
    }

    fn set_ring_segment(&mut self, encoder: u8, segment: u8, color: Rgb) -> Result<(), Error> {
        self.deck
            .encoder(encoder)?
            .ring()
            .set_segment(segment, color)
    }

    fn draw_lcd_jpeg(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        jpeg: &[u8],
    ) -> Result<(), Error> {
        self.deck.lcd().draw_jpeg_at(x, y, width, height, jpeg)
    }

    fn clear_all(&mut self) -> Result<(), Error> {
        self.deck.clear_all()
    }

    fn reset_to_logo(&mut self) -> Result<(), Error> {
        self.deck.reset_to_logo()
    }
}
