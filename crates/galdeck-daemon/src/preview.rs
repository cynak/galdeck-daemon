//! What the deck is showing, for anything that wants to watch.
//!
//! The daemon already renders every key to a JPEG on its way to the panel, so
//! a browser preview is that same JPEG served over HTTP rather than a second
//! renderer that could disagree with the first. It is genuinely what the
//! hardware was sent, which is what makes it worth calling a preview.
//!
//! Shared through a mutex rather than a channel because readers want the
//! latest state, not every state.

use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use galdeck::keyboard::LightFrame;
use galdeck::{Buttons, Encoders, Rgb, Ring};
use galdeck_ipc::Event;

/// The most recent rendering of each surface.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub keys: Vec<Option<Arc<[u8]>>>,
    pub lcd: Option<Arc<[u8]>>,
    pub rings: Vec<[Rgb; Ring::SEGMENTS as usize]>,
    pub brightness: u8,
    /// Bumped on every change, so a client can tell whether to re-fetch.
    pub generation: u64,
}

impl Frame {
    fn new() -> Self {
        Self {
            keys: vec![None; Buttons::COUNT as usize],
            lcd: None,
            rings: vec![[Rgb::BLACK; Ring::SEGMENTS as usize]; Encoders::COUNT as usize],
            brightness: 0,
            generation: 0,
        }
    }
}

/// Shared preview state plus the list of clients watching for events.
#[derive(Clone)]
pub struct Preview {
    frame: Arc<Mutex<Frame>>,
    subscribers: Arc<Mutex<Vec<SyncSender<Event>>>>,
    /// What the keyboard's lighting was last sent, while the daemon lights it.
    keyboard: Arc<Mutex<Option<LightFrame>>>,
}

impl Default for Preview {
    fn default() -> Self {
        Self::new()
    }
}

impl Preview {
    pub fn new() -> Self {
        Self {
            frame: Arc::new(Mutex::new(Frame::new())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            keyboard: Arc::new(Mutex::new(None)),
        }
    }

    /// What the keyboard's lighting shows, or `None` while the daemon is not
    /// lighting it.
    pub fn keyboard(&self) -> Option<LightFrame> {
        self.keyboard.lock().expect("preview poisoned").clone()
    }

    pub fn set_keyboard(&self, frame: Option<LightFrame>) {
        *self.keyboard.lock().expect("preview poisoned") = frame;
    }

    pub fn frame(&self) -> Frame {
        self.frame.lock().expect("preview poisoned").clone()
    }

    pub fn key(&self, index: u8) -> Option<Arc<[u8]>> {
        self.frame
            .lock()
            .expect("preview poisoned")
            .keys
            .get(index as usize)
            .cloned()
            .flatten()
    }

    pub fn lcd(&self) -> Option<Arc<[u8]>> {
        self.frame.lock().expect("preview poisoned").lcd.clone()
    }

    pub fn set_key(&self, index: u8, jpeg: Option<Arc<[u8]>>) {
        let mut frame = self.frame.lock().expect("preview poisoned");
        if let Some(slot) = frame.keys.get_mut(index as usize) {
            *slot = jpeg;
        }
        frame.generation += 1;
    }

    pub fn set_lcd(&self, jpeg: Arc<[u8]>) {
        let mut frame = self.frame.lock().expect("preview poisoned");
        frame.lcd = Some(jpeg);
        frame.generation += 1;
    }

    pub fn set_ring(&self, encoder: u8, colors: [Rgb; Ring::SEGMENTS as usize]) {
        let mut frame = self.frame.lock().expect("preview poisoned");
        if let Some(slot) = frame.rings.get_mut(encoder as usize) {
            *slot = colors;
        }
        frame.generation += 1;
    }

    pub fn set_brightness(&self, percent: u8) {
        let mut frame = self.frame.lock().expect("preview poisoned");
        frame.brightness = percent;
        frame.generation += 1;
    }

    /// Watch for events. The receiver is dropped when the client goes away,
    /// which is how the subscription ends.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<Event> {
        // Bounded: a client that stops reading must not be able to grow the
        // daemon's memory. Dropped events are acceptable -- every one of them
        // is a hint to re-read state, not the state itself.
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        self.subscribers
            .lock()
            .expect("subscribers poisoned")
            .push(tx);
        rx
    }

    /// Tell every watcher, and forget the ones that have gone.
    pub fn publish(&self, event: Event) {
        self.subscribers
            .lock()
            .expect("subscribers poisoned")
            .retain(|tx| match tx.try_send(event.clone()) {
                Ok(()) => true,
                // Behind, but still there: keep it and drop this one.
                Err(TrySendError::Full(_)) => true,
                Err(TrySendError::Disconnected(_)) => false,
            });
    }

    pub fn watchers(&self) -> usize {
        self.subscribers.lock().expect("subscribers poisoned").len()
    }
}
