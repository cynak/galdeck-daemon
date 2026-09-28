//! The real keyboard's lighting.
//!
//! The only file in the workspace that names `galdeck::keyboard::Keyboard`;
//! everything else goes through [`Lights`], as the deck goes through
//! [`Deck`](crate::Deck).

use std::time::Duration;

use galdeck::keyboard::{KeyChange, KeyReader, Keyboard, LightFrame};
use galdeck::Error;

use crate::{to_light, Lights};

pub struct HardwareLights {
    keyboard: Keyboard,
    firmware: String,
    /// Its key presses, when the udev rule's opt-in line lets them be read.
    keys: Option<KeyReader>,
}

impl HardwareLights {
    /// Open the first Galleon keyboard the system reports and take over its
    /// lighting. Blocks about half a second while the keyboard settles, so it
    /// belongs on the lighting thread, never on one that answers requests.
    pub fn open(api: &galdeck::hidapi::HidApi) -> Result<Self, Error> {
        let mut keyboard = Keyboard::open(api)?;
        // Read once: a property read, and nothing about it changes while the
        // handle is open.
        let firmware = keyboard
            .firmware_version()
            .unwrap_or_else(|_| "unknown".to_string());
        // Opened after the lighting is taken over: the keyboard reports keys
        // only then. Without the opt-in the lighting simply does not answer
        // key presses; nothing else changes.
        let keys = KeyReader::open(api).ok();
        Ok(Self {
            keyboard,
            firmware,
            keys,
        })
    }

    pub fn firmware(&self) -> &str {
        &self.firmware
    }
}

impl Lights for HardwareLights {
    fn show(&mut self, frame: &LightFrame) -> Result<(), Error> {
        self.keyboard.show(&to_light(frame))
    }

    fn tick_keepalive(&mut self) -> Result<(), Error> {
        self.keyboard.tick_keepalive()
    }

    fn release(self: Box<Self>) -> Result<(), Error> {
        self.keyboard.release()
    }

    fn reports_keys(&self) -> bool {
        self.keys.is_some()
    }

    fn key_changes(&mut self, wait: Duration) -> Result<Vec<KeyChange>, Error> {
        match &mut self.keys {
            Some(keys) => keys.poll(wait),
            None => Ok(Vec::new()),
        }
    }
}
