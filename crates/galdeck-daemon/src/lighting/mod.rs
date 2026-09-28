//! The keyboard lighting thread.
//!
//! One thread owns the keyboard's lighting. It takes the lighting over when
//! the profile showing asks for it, draws the effect, keeps the keyboard in
//! software mode, reconnects when the keyboard is unplugged, and hands the
//! lighting back when nothing asks for it any more or the daemon stops. The
//! keyboard never takes its lighting back by itself, so handing back is not
//! optional.
//!
//! It is a thread of its own because a frame blocks for several milliseconds
//! on the keyboard's acknowledgements, and opening the keyboard for half a
//! second: neither may hold up the deck or the engine. The engine only sends
//! it the resolved lighting whenever that changes, and never waits on it.
//!
//! When the lighting answers key presses, the thread waits on the keyboard
//! rather than on the engine, so a press lights up at once. It keeps only
//! where each pressed key is and when, for as long as the press shows, and
//! never logs a press.

pub mod editor;
pub mod effects;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use galdeck::keyboard::{KeyChange, LightFrame};
use galdeck_device::Lights;
use galdeck_model::{LightingEffect, ResolvedLighting};

use crate::preview::Preview;
use effects::{Press, Renderer};

/// Opens the keyboard's lighting: the real one, or a fake in tests and under
/// `--device virtual`.
pub type Opener = Box<dyn FnMut() -> Result<Box<dyn Lights>, galdeck::Error> + Send>;

/// The lighting thread's pace.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Between frames while the picture moves or fades.
    pub frame: Duration,
    /// How long a change of lighting takes to fade in.
    pub fade: Duration,
    /// Between attempts to open a keyboard that is not there.
    pub reconnect: Duration,
    /// The longest the thread goes without looking at the shutdown flag and
    /// the keepalive.
    pub idle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            frame: Duration::from_micros(16_667),
            fade: Duration::from_millis(300),
            reconnect: Duration::from_secs(2),
            idle: Duration::from_millis(250),
        }
    }
}

/// Where the engine sends lighting.
#[derive(Clone)]
pub struct LightingHandle {
    tx: Sender<Option<ResolvedLighting>>,
}

impl LightingHandle {
    /// Light the keyboard this way from now on. `None`, or an effect of
    /// `off`, hands the lighting back to the keyboard.
    pub fn set(&self, lighting: Option<ResolvedLighting>) {
        // A thread that has gone has already handed the lighting back.
        let _ = self.tx.send(lighting);
    }
}

/// Start the lighting thread. It runs until `shutdown` is set or every
/// [`LightingHandle`] is dropped, and hands the lighting back as it ends.
/// What the keyboard shows is published to `preview`, for the configuration
/// UI.
pub fn spawn(
    open: Opener,
    shutdown: Arc<AtomicBool>,
    timing: Timing,
    preview: Preview,
) -> std::io::Result<(LightingHandle, JoinHandle<()>)> {
    let (tx, rx) = channel();
    let mut thread = LightingThread::new(rx, open, shutdown, timing, preview);
    let join = std::thread::Builder::new()
        .name("galdeck-lights".into())
        .spawn(move || thread.run())?;
    Ok((LightingHandle { tx }, join))
}

struct LightingThread {
    rx: Receiver<Option<ResolvedLighting>>,
    open: Opener,
    /// Where what the keyboard shows is published.
    preview: Preview,
    shutdown: Arc<AtomicBool>,
    timing: Timing,
    /// Where effect time is measured from, so a change of lighting does not
    /// restart a wave.
    started: Instant,
    /// What to draw. `None` leaves the keyboard alone.
    renderer: Option<Renderer>,
    lights: Option<Box<dyn Lights>>,
    /// The frame on the keyboard, if it is still current. A still picture is
    /// drawn once and then only kept alive.
    shown: Option<LightFrame>,
    /// The frame a change fades from, and when the fade began.
    fading: Option<(LightFrame, Instant)>,
    next_frame: Instant,
    next_attempt: Instant,
    /// Why the keyboard last failed to open, so a machine without one -- or
    /// without the udev rule -- hears about it once, not every two seconds.
    open_failure: Option<String>,
    /// Presses the lighting is still answering: where each key is, and when.
    presses: Vec<Press>,
    /// Whether it has said that this keyboard's presses cannot be read, which
    /// it says once.
    told_no_keys: bool,
}

impl LightingThread {
    fn new(
        rx: Receiver<Option<ResolvedLighting>>,
        open: Opener,
        shutdown: Arc<AtomicBool>,
        timing: Timing,
        preview: Preview,
    ) -> Self {
        let now = Instant::now();
        LightingThread {
            rx,
            open,
            preview,
            shutdown,
            timing,
            started: now,
            renderer: None,
            lights: None,
            shown: None,
            fading: None,
            next_frame: now,
            next_attempt: now,
            open_failure: None,
            presses: Vec::new(),
            told_no_keys: false,
        }
    }

    fn run(&mut self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            let wait = self.wait();
            let lighting = if self.listens_for_keys() {
                // Waiting on the keyboard rather than on the engine: a press
                // lights up at once, and a change of lighting can wait a
                // moment.
                self.read_keys(wait);
                match self.rx.try_recv() {
                    Ok(lighting) => Some(lighting),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => break,
                }
            } else {
                match self.rx.recv_timeout(wait) {
                    Ok(lighting) => Some(lighting),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            if let Some(mut lighting) = lighting {
                // Only the latest matters; a burst of reloads is one change.
                while let Ok(later) = self.rx.try_recv() {
                    lighting = later;
                }
                self.apply(lighting);
            }
            self.step();
        }
        self.hand_back();
    }

    /// Whether presses light anything, and the keyboard reports them.
    fn listens_for_keys(&self) -> bool {
        self.renderer
            .as_ref()
            .is_some_and(|r| r.reaction().is_some())
            && self.lights.as_ref().is_some_and(|l| l.reports_keys())
    }

    /// Wait up to `wait` for key presses, and start answering any.
    fn read_keys(&mut self, wait: Duration) {
        let Some(lights) = self.lights.as_mut() else {
            return;
        };
        match lights.key_changes(wait) {
            Ok(changes) => {
                let t = self.started.elapsed().as_secs_f64();
                let before = self.presses.len();
                self.presses
                    .extend(changes.into_iter().filter_map(|change| match change {
                        KeyChange::Down(led) => Press::new(led, t),
                        KeyChange::Up(_) => None,
                    }));
                if self.presses.len() > before {
                    // Now, not whenever the next frame was due.
                    self.next_frame = Instant::now();
                }
            }
            Err(error) => self.lost(error),
        }
    }

    /// How long to wait for the engine before the next thing to do.
    fn wait(&self) -> Duration {
        let until = match (&self.renderer, &self.lights) {
            (None, _) => return self.timing.idle,
            (Some(_), None) => self.next_attempt,
            (Some(renderer), Some(_)) => {
                if renderer.moves()
                    || self.fading.is_some()
                    || self.shown.is_none()
                    || !self.presses.is_empty()
                {
                    self.next_frame
                } else {
                    return self.timing.idle;
                }
            }
        };
        until
            .saturating_duration_since(Instant::now())
            .min(self.timing.idle)
    }

    fn apply(&mut self, lighting: Option<ResolvedLighting>) {
        let Some(lighting) = lighting.filter(|l| l.effect != LightingEffect::Off) else {
            self.renderer = None;
            self.presses.clear();
            return;
        };
        let (renderer, unknown) = Renderer::new(lighting);
        if !unknown.is_empty() {
            log::warn!("lighting: no key named {}", unknown.join(", "));
        }
        if self.renderer.as_ref() == Some(&renderer) {
            // A reload that left the lighting as it was.
            return;
        }
        if renderer.reaction().is_none() {
            self.presses.clear();
        }
        self.renderer = Some(renderer);
        // Fade from whatever the keyboard shows now.
        if let Some(from) = self.shown.take() {
            self.fading = Some((from, Instant::now()));
        }
        self.next_frame = Instant::now();
    }

    fn step(&mut self) {
        if self.renderer.is_none() {
            self.hand_back();
            return;
        }
        if self.lights.is_none() {
            if Instant::now() < self.next_attempt {
                return;
            }
            self.connect();
            if self.lights.is_none() {
                return;
            }
        }

        if !self.told_no_keys
            && self
                .renderer
                .as_ref()
                .is_some_and(|r| r.reaction().is_some())
            && self.lights.as_ref().is_some_and(|l| !l.reports_keys())
        {
            log::info!(
                "lighting is set to answer key presses, but this keyboard's cannot be read; \
                 the udev rule's opt-in line for its key reports allows it"
            );
            self.told_no_keys = true;
        }

        let now = Instant::now();
        let t = now.duration_since(self.started).as_secs_f64();
        if let Some((_, since)) = &self.fading {
            if now.duration_since(*since) >= self.timing.fade {
                self.fading = None;
                // Land on the lighting itself: the last fading frame fell
                // just short of it, and a still picture is never redrawn.
                self.shown = None;
            }
        }
        // Presses that have faded are forgotten, and the last to go takes its
        // light with it in one more frame.
        if let Some(reaction) = self.renderer.as_ref().and_then(Renderer::reaction) {
            let answering = !self.presses.is_empty();
            self.presses.retain(|press| !press.is_over(t, reaction));
            if answering && self.presses.is_empty() {
                self.shown = None;
            }
        }
        let Some(renderer) = &self.renderer else {
            return;
        };
        let due = renderer.moves()
            || self.fading.is_some()
            || self.shown.is_none()
            || !self.presses.is_empty();
        if due && now >= self.next_frame {
            let mut frame = LightFrame::new();
            renderer.render(t, &self.presses, &mut frame);
            if let Some((from, since)) = &self.fading {
                let progress = now.duration_since(*since).as_secs_f32()
                    / self.timing.fade.as_secs_f32().max(f32::EPSILON);
                frame = effects::blend(from, &frame, ease(progress));
            }
            let shown = match self.lights.as_mut() {
                Some(lights) => lights.show(&frame),
                None => return,
            };
            if let Err(error) = shown {
                self.lost(error);
                return;
            }
            self.preview.set_keyboard(Some(frame.clone()));
            self.shown = Some(frame);
            let next = self.next_frame + self.timing.frame;
            // Behind, as after a slow frame: carry on from now rather than
            // rushing out the frames that were missed.
            self.next_frame = next.max(now);
        }

        let kept = match self.lights.as_mut() {
            Some(lights) => lights.tick_keepalive(),
            None => return,
        };
        if let Err(error) = kept {
            self.lost(error);
        }
    }

    fn connect(&mut self) {
        self.next_attempt = Instant::now() + self.timing.reconnect;
        match (self.open)() {
            Ok(lights) => {
                log::info!("keyboard lighting taken over");
                self.lights = Some(lights);
                self.open_failure = None;
                // Fade in from dark rather than snapping on.
                self.shown = None;
                self.fading = Some((LightFrame::new(), Instant::now()));
                self.next_frame = Instant::now();
            }
            Err(error) => {
                let failure = error.to_string();
                if self.open_failure.as_ref() != Some(&failure) {
                    match error {
                        galdeck::Error::KeyboardNotFound => {
                            log::info!("no keyboard lighting found; will keep looking")
                        }
                        _ => log::warn!("could not open the keyboard's lighting: {failure}"),
                    }
                    self.open_failure = Some(failure);
                }
            }
        }
    }

    /// The keyboard stopped answering: most likely unplugged.
    fn lost(&mut self, error: galdeck::Error) {
        log::warn!("lost the keyboard's lighting: {error}");
        // Dropping tries to hand the lighting back, in case it was a glitch
        // rather than an unplug.
        self.lights = None;
        self.shown = None;
        self.fading = None;
        self.preview.set_keyboard(None);
        self.next_attempt = Instant::now() + self.timing.reconnect;
    }

    fn hand_back(&mut self) {
        self.shown = None;
        self.fading = None;
        self.preview.set_keyboard(None);
        if let Some(lights) = self.lights.take() {
            match lights.release() {
                Ok(()) => log::info!("keyboard lighting handed back"),
                Err(error) => log::warn!("could not hand the keyboard's lighting back: {error}"),
            }
        }
    }
}

/// Slow at both ends, so a fade neither jumps off nor lands abruptly.
fn ease(progress: f32) -> f32 {
    let p = progress.clamp(0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use galdeck::keyboard::Led;
    use galdeck::Rgb;
    use galdeck_device::{FakeLights, FakeLightsHandle};
    use galdeck_model::lighting::{ReactiveEffect, ResolvedReactive};
    use std::sync::Mutex;

    /// Fast enough that a test takes milliseconds, slow enough to be stable.
    const FAST: Timing = Timing {
        frame: Duration::from_millis(2),
        fade: Duration::from_millis(20),
        reconnect: Duration::from_millis(20),
        idle: Duration::from_millis(5),
    };

    /// Every keyboard a test's thread has opened, newest last.
    type Opened = Arc<Mutex<Vec<FakeLightsHandle>>>;

    /// Every keyboard the thread has opened, newest last. The first `absent`
    /// attempts find nothing, as on a machine with the keyboard unplugged.
    fn opener(absent: usize) -> (Opener, Opened) {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&opened);
        let mut attempts = 0;
        let open: Opener = Box::new(move || {
            attempts += 1;
            if attempts <= absent {
                return Err(galdeck::Error::KeyboardNotFound);
            }
            let (lights, handle) = FakeLights::new();
            record.lock().unwrap().push(handle);
            Ok(Box::new(lights) as Box<dyn Lights>)
        });
        (open, opened)
    }

    fn solid(color: Rgb) -> Option<ResolvedLighting> {
        Some(ResolvedLighting {
            effect: LightingEffect::Static,
            colors: vec![color],
            speed: 0.2,
            brightness: 100,
            bar: None,
            keys: Vec::new(),
            reactive: None,
        })
    }

    fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn showing(opened: &Opened, which: usize, color: Rgb) -> bool {
        let opened = opened.lock().unwrap();
        let a = Led::key("A").unwrap();
        opened
            .get(which)
            .and_then(FakeLightsHandle::last_frame)
            .is_some_and(|frame| frame.get(a) == color)
    }

    fn start(absent: usize) -> (LightingHandle, JoinHandle<()>, Arc<AtomicBool>, Opened) {
        let (open, opened) = opener(absent);
        let shutdown = Arc::new(AtomicBool::new(false));
        let (handle, join) = spawn(open, Arc::clone(&shutdown), FAST, Preview::new()).unwrap();
        (handle, join, shutdown, opened)
    }

    #[test]
    fn leaves_the_keyboard_alone_until_asked() {
        let (handle, join, shutdown, opened) = start(0);
        std::thread::sleep(Duration::from_millis(30));
        assert!(opened.lock().unwrap().is_empty());
        handle.set(Some(ResolvedLighting {
            effect: LightingEffect::Off,
            ..solid(Rgb::RED).unwrap()
        }));
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            opened.lock().unwrap().is_empty(),
            "`off` took the keyboard over"
        );
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }

    #[test]
    fn takes_over_fades_in_and_hands_back() {
        let (handle, join, shutdown, opened) = start(0);
        handle.set(solid(Rgb::RED));
        eventually("the keyboard shows red", || showing(&opened, 0, Rgb::RED));
        // It faded in: something dimmer was shown first.
        assert!(opened.lock().unwrap()[0].frames_shown() > 1);

        handle.set(None);
        eventually("the lighting is handed back", || {
            opened.lock().unwrap()[0].released()
        });
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }

    #[test]
    fn a_still_picture_is_drawn_once_then_only_kept_alive() {
        let (handle, join, shutdown, opened) = start(0);
        handle.set(solid(Rgb::RED));
        eventually("the keyboard shows red", || showing(&opened, 0, Rgb::RED));
        let frames = opened.lock().unwrap()[0].frames_shown();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(opened.lock().unwrap()[0].frames_shown(), frames);
        assert!(opened.lock().unwrap()[0].keepalives() > 0);
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }

    #[test]
    fn a_change_fades_to_the_new_lighting() {
        let (handle, join, shutdown, opened) = start(0);
        handle.set(solid(Rgb::RED));
        eventually("red", || showing(&opened, 0, Rgb::RED));
        handle.set(solid(Rgb::BLUE));
        eventually("blue", || showing(&opened, 0, Rgb::BLUE));
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
        assert_eq!(
            opened.lock().unwrap().len(),
            1,
            "a change reopened the keyboard"
        );
    }

    #[test]
    fn shutting_down_hands_the_lighting_back() {
        let (handle, join, shutdown, opened) = start(0);
        handle.set(solid(Rgb::RED));
        eventually("red", || showing(&opened, 0, Rgb::RED));
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
        assert!(opened.lock().unwrap()[0].released());
    }

    #[test]
    fn the_engine_going_away_hands_the_lighting_back() {
        let (handle, join, _shutdown, opened) = start(0);
        handle.set(solid(Rgb::RED));
        eventually("red", || showing(&opened, 0, Rgb::RED));
        drop(handle);
        join.join().unwrap();
        assert!(opened.lock().unwrap()[0].released());
    }

    #[test]
    fn keeps_looking_for_a_keyboard_that_is_not_there() {
        let (handle, join, shutdown, opened) = start(3);
        handle.set(solid(Rgb::RED));
        eventually("the keyboard, once found, shows red", || {
            showing(&opened, 0, Rgb::RED)
        });
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }

    #[test]
    fn reconnects_after_an_unplug() {
        let (handle, join, shutdown, opened) = start(0);
        handle.set(solid(Rgb::RED));
        eventually("red", || showing(&opened, 0, Rgb::RED));
        opened.lock().unwrap()[0].unplug();
        eventually("a second keyboard shows red", || {
            showing(&opened, 1, Rgb::RED)
        });
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }

    /// The dark keyboard, answering presses with `effect` in white.
    fn answering(effect: ReactiveEffect) -> Option<ResolvedLighting> {
        Some(ResolvedLighting {
            reactive: Some(ResolvedReactive {
                effect,
                color: Rgb::WHITE,
                fade_ms: 300,
            }),
            ..solid(Rgb::BLACK).unwrap()
        })
    }

    /// Whether the keyboard `which` has stopped being drawn: taken over,
    /// faded in, and still.
    fn still(opened: &Opened, which: usize) -> bool {
        let frames = || {
            opened
                .lock()
                .unwrap()
                .get(which)
                .map(FakeLightsHandle::frames_shown)
        };
        let before = frames();
        std::thread::sleep(FAST.fade + FAST.frame * 4);
        before.is_some() && frames() == before
    }

    /// How bright A is on the newest frame the keyboard `which` was shown.
    fn a_level(opened: &Opened, which: usize) -> Option<u8> {
        let a = Led::key("A").unwrap();
        let frame = opened.lock().unwrap().get(which)?.last_frame()?;
        Some(frame.get(a).r)
    }

    #[test]
    fn a_press_lights_up_fades_and_leaves_the_keyboard_still() {
        let (handle, join, shutdown, opened) = start(0);
        handle.set(answering(ReactiveEffect::Glow));
        // Pressed once the takeover has faded in: a press during that fade
        // fades in with everything else, which is right, but not this test.
        eventually("the keyboard is dark and still", || {
            showing(&opened, 0, Rgb::BLACK) && still(&opened, 0)
        });
        opened.lock().unwrap()[0].press(Led::key("A").unwrap());
        eventually("A lights up", || {
            a_level(&opened, 0).is_some_and(|r| r > 120)
        });
        eventually("A fades out", || showing(&opened, 0, Rgb::BLACK));
        // Once the press is over, a still keyboard stops being drawn.
        eventually("the keyboard is still again", || still(&opened, 0));
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }

    #[test]
    fn without_the_opt_in_a_press_lights_nothing() {
        let (handle, join, shutdown, opened) = start(0);
        handle.set(answering(ReactiveEffect::Ripple));
        eventually("the keyboard is dark and still", || {
            showing(&opened, 0, Rgb::BLACK) && still(&opened, 0)
        });
        let lights = opened.lock().unwrap()[0].clone();
        lights.stop_reporting_keys();
        lights.press(Led::key("A").unwrap());
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(a_level(&opened, 0), Some(0));
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }

    #[test]
    fn what_the_keyboard_shows_is_published_until_it_is_handed_back() {
        let (open, opened) = opener(0);
        let shutdown = Arc::new(AtomicBool::new(false));
        let preview = Preview::new();
        let (handle, join) = spawn(open, Arc::clone(&shutdown), FAST, preview.clone()).unwrap();
        assert!(preview.keyboard().is_none());
        handle.set(solid(Rgb::RED));
        eventually("red", || showing(&opened, 0, Rgb::RED));
        let a = Led::key("A").unwrap();
        eventually("the preview shows red", || {
            preview
                .keyboard()
                .is_some_and(|frame| frame.get(a) == Rgb::RED)
        });
        handle.set(None);
        eventually("the preview is cleared", || preview.keyboard().is_none());
        shutdown.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }

    #[test]
    fn ease_starts_and_ends_still() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(0.5), 0.5);
        assert_eq!(ease(2.0), 1.0);
    }
}
