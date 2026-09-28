//! The keyboard-lighting seam.
//!
//! The daemon lights the keyboard through the [`Lights`] trait rather than
//! `galdeck::keyboard::Keyboard` directly, for the reason it draws through
//! [`Deck`](crate::Deck): the lighting thread and its effects then run
//! against [`FakeLights`] on a machine with no keyboard, which is what makes
//! them testable in CI.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use galdeck::keyboard::{KeyChange, Led, LightFrame};
use galdeck::{Error, Rgb};

/// How an LED answers the level it is sent: its light goes as the level to
/// this power, where a screen's does as the level itself. See [`to_light`].
pub const LED_GAMMA: f32 = 2.2;

/// The level that makes an LED give the light a screen gives for `level`.
///
/// Frames are drawn as a screen shows colour, and an LED's light goes as the
/// level it is sent: sent unchanged, a colour's weaker channels shine many
/// times too bright, and every colour but a pure one washes out toward
/// white. `#ff2222` is a deep red on screen, and pink on the keys.
pub fn led_level(level: u8) -> u8 {
    let light = (f32::from(level) / 255.0).powf(LED_GAMMA);
    (light * 255.0).round() as u8
}

/// A frame as a screen shows it, as the levels that make the keyboard's
/// LEDs show the same.
pub fn to_light(frame: &LightFrame) -> LightFrame {
    let mut out = LightFrame::new();
    for led in Led::all() {
        let color = frame.get(led);
        out.set(
            led,
            Rgb::new(led_level(color.r), led_level(color.g), led_level(color.b)),
        );
    }
    out
}

/// The keyboard's lighting, taken over.
///
/// While one is held the keyboard shows only what [`Lights::show`] sends.
/// The keyboard never takes its lighting back by itself, so an implementation
/// hands it back in [`Lights::release`] and again, best effort, when dropped.
pub trait Lights: Send {
    /// Show one frame, drawn as a screen would show it: one that drives LEDs
    /// turns it into their levels with [`to_light`]. Blocks for a few
    /// milliseconds.
    fn show(&mut self, frame: &LightFrame) -> Result<(), Error>;

    /// Keep the keyboard in software mode. Call it regularly, frames or not.
    fn tick_keepalive(&mut self) -> Result<(), Error>;

    /// Hand the lighting back to the keyboard's own effects.
    fn release(self: Box<Self>) -> Result<(), Error>;

    /// Whether key presses can be read, for lighting that answers them. A
    /// real keyboard's can only with the udev rule's opt-in line, since
    /// they are every key press on it.
    fn reports_keys(&self) -> bool {
        false
    }

    /// The keys that went down or came up since the last call, waiting up
    /// to `wait` for the first. Returns at once, with nothing, from lights
    /// that do not report keys.
    fn key_changes(&mut self, wait: Duration) -> Result<Vec<KeyChange>, Error> {
        let _ = wait;
        Ok(Vec::new())
    }
}

/// What a [`FakeLights`] has been asked to do.
#[derive(Debug)]
struct FakeState {
    shown: Vec<LightFrame>,
    keepalives: usize,
    released: bool,
    /// Every call fails as a vanished keyboard would.
    unplugged: bool,
    reports_keys: bool,
    /// Key changes put in by a test, waiting to be read.
    pending: VecDeque<KeyChange>,
}

impl Default for FakeState {
    fn default() -> Self {
        FakeState {
            shown: Vec::new(),
            keepalives: 0,
            released: false,
            unplugged: false,
            reports_keys: true,
            pending: VecDeque::new(),
        }
    }
}

/// The state, and a signal for when a key change is put in, so that
/// waiting for one sleeps as the real read does rather than spinning.
#[derive(Default)]
struct Shared {
    state: Mutex<FakeState>,
    keys: Condvar,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Lights that exist only in memory, for tests and `--device virtual`.
/// They report keys, which a test puts in through the handle.
pub struct FakeLights {
    shared: Arc<Shared>,
}

/// Watches a [`FakeLights`] from outside the thread that owns it.
#[derive(Clone)]
pub struct FakeLightsHandle {
    shared: Arc<Shared>,
}

impl FakeLights {
    pub fn new() -> (Self, FakeLightsHandle) {
        let shared = Arc::new(Shared::default());
        (
            FakeLights {
                shared: Arc::clone(&shared),
            },
            FakeLightsHandle { shared },
        )
    }
}

impl FakeLightsHandle {
    /// How many frames have been shown.
    pub fn frames_shown(&self) -> usize {
        self.shared.state().shown.len()
    }

    /// The frame showing now, if any.
    pub fn last_frame(&self) -> Option<LightFrame> {
        self.shared.state().shown.last().cloned()
    }

    pub fn keepalives(&self) -> usize {
        self.shared.state().keepalives
    }

    /// Whether the lighting has been handed back.
    pub fn released(&self) -> bool {
        self.shared.state().released
    }

    /// Make every later call fail, as pulling the keyboard's cable would.
    pub fn unplug(&self) {
        self.shared.state().unplugged = true;
        self.shared.keys.notify_all();
    }

    /// Press a key and let it go, as a quick tap does.
    pub fn press(&self, led: Led) {
        {
            let mut state = self.shared.state();
            state.pending.push_back(KeyChange::Down(led));
            state.pending.push_back(KeyChange::Up(led));
        }
        self.shared.keys.notify_all();
    }

    /// Stop reporting keys, as a keyboard without the udev opt-in does.
    pub fn stop_reporting_keys(&self) {
        self.shared.state().reports_keys = false;
    }
}

fn gone() -> Error {
    Error::KeyboardNotFound
}

impl Lights for FakeLights {
    fn show(&mut self, frame: &LightFrame) -> Result<(), Error> {
        let mut state = self.shared.state();
        if state.unplugged {
            return Err(gone());
        }
        state.shown.push(frame.clone());
        Ok(())
    }

    fn tick_keepalive(&mut self) -> Result<(), Error> {
        let mut state = self.shared.state();
        if state.unplugged {
            return Err(gone());
        }
        state.keepalives += 1;
        Ok(())
    }

    fn release(self: Box<Self>) -> Result<(), Error> {
        let unplugged = {
            let mut state = self.shared.state();
            state.released = true;
            state.unplugged
        };
        if unplugged {
            Err(gone())
        } else {
            Ok(())
        }
    }

    fn reports_keys(&self) -> bool {
        self.shared.state().reports_keys
    }

    fn key_changes(&mut self, wait: Duration) -> Result<Vec<KeyChange>, Error> {
        let mut state = self.shared.state();
        if !state.reports_keys && !state.unplugged {
            return Ok(Vec::new());
        }
        if state.pending.is_empty() && !state.unplugged {
            state = self
                .shared
                .keys
                .wait_timeout(state, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        if state.unplugged {
            return Err(gone());
        }
        // Asked again after waiting: keys it has stopped reporting stay unsaid.
        if !state.reports_keys {
            return Ok(Vec::new());
        }
        Ok(state.pending.drain(..).collect())
    }
}

impl Drop for FakeLights {
    // A real keyboard is handed back when its handle drops; so is this, so a
    // test sees the same end state whichever way the handle went away.
    fn drop(&mut self) {
        self.shared.state().released = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use galdeck::Rgb;
    use std::time::Instant;

    #[test]
    fn records_frames_and_release() {
        let (lights, handle) = FakeLights::new();
        let mut lights: Box<dyn Lights> = Box::new(lights);
        lights.show(&LightFrame::filled(Rgb::RED)).unwrap();
        lights.tick_keepalive().unwrap();
        assert_eq!(handle.frames_shown(), 1);
        assert_eq!(handle.keepalives(), 1);
        assert!(!handle.released());
        lights.release().unwrap();
        assert!(handle.released());
    }

    #[test]
    fn unplugged_fails_every_call() {
        let (mut lights, handle) = FakeLights::new();
        handle.unplug();
        assert!(lights.show(&LightFrame::new()).is_err());
        assert!(lights.tick_keepalive().is_err());
        assert!(lights.key_changes(Duration::ZERO).is_err());
    }

    #[test]
    fn dropping_hands_the_lighting_back() {
        let (lights, handle) = FakeLights::new();
        drop(lights);
        assert!(handle.released());
    }

    #[test]
    fn a_press_is_read_as_a_down_and_an_up() {
        let (mut lights, handle) = FakeLights::new();
        let a = Led::key("A").unwrap();
        handle.press(a);
        assert_eq!(
            lights.key_changes(Duration::ZERO).unwrap(),
            [KeyChange::Down(a), KeyChange::Up(a)]
        );
        assert!(lights.key_changes(Duration::ZERO).unwrap().is_empty());
    }

    #[test]
    fn waiting_for_keys_sleeps_until_one_comes() {
        let (mut lights, handle) = FakeLights::new();
        let started = Instant::now();
        assert!(lights
            .key_changes(Duration::from_millis(30))
            .unwrap()
            .is_empty());
        assert!(started.elapsed() >= Duration::from_millis(25), "it spun");

        let a = Led::key("A").unwrap();
        let presser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            handle.press(a);
        });
        let started = Instant::now();
        let changes = lights.key_changes(Duration::from_secs(5)).unwrap();
        presser.join().unwrap();
        assert!(!changes.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "it slept through the press"
        );
    }

    #[test]
    fn off_and_full_are_the_same_on_a_screen_and_an_led() {
        assert_eq!(led_level(0), 0);
        assert_eq!(led_level(255), 255);
        assert!((1..=255).all(|level| led_level(level) >= led_level(level - 1)));
    }

    #[test]
    fn a_colours_weak_channels_stay_weak_on_the_keys() {
        // #ff2222: on screen, its green and blue give about 1% of full
        // light. Sent as they are, an LED would give 13%, washing the red
        // out to pink.
        assert!(led_level(0x22) <= 4, "{}", led_level(0x22));
        // A screen's half level is a fifth of its light.
        assert_eq!(led_level(128), 56);
    }

    #[test]
    fn a_frame_is_turned_into_light_led_by_led() {
        let mut frame = LightFrame::filled(Rgb::new(0xff, 0x22, 0x22));
        let bar = Led::bar(6).unwrap();
        frame.set(bar, Rgb::new(0x88, 0xc0, 0xd0));
        let light = to_light(&frame);
        assert_eq!(light.get(Led::key("A").unwrap()), Rgb::new(255, 3, 3));
        assert_eq!(light.get(bar), Rgb::new(64, 137, 163));
    }

    #[test]
    fn without_the_opt_in_no_keys_are_reported() {
        let (mut lights, handle) = FakeLights::new();
        handle.stop_reporting_keys();
        assert!(!lights.reports_keys());
        handle.press(Led::key("A").unwrap());
        // Put in, but a keyboard that does not report keys says nothing,
        // and at once.
        let started = Instant::now();
        assert!(lights
            .key_changes(Duration::from_millis(500))
            .unwrap()
            .is_empty());
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
