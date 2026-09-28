//! A theme's `[motion]`, driven through a fake deck: a key's answer to a
//! press, an alarm that moves, and knob rings at rest.
//!
//! Real time, as the states and timers tests use: each check waits for what
//! it expects rather than assuming when a frame lands. The only command run
//! is `echo`, for a widget's reading.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use galdeck::{Event, Rgb};
use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine, EngineParts};
use galdeck_daemon::io::IoThread;
use galdeck_daemon::plugins::PluginHost;
use galdeck_daemon::preview::Preview;
use galdeck_daemon::widgets::WidgetHost;
use galdeck_device::{FakeDeck, FakeDeckHandle};
use galdeck_ipc::{Request, Response, Status};
use galdeck_model::Workspace;

struct Harness {
    control: ControlSender,
    deck: FakeDeckHandle,
    preview: Preview,
    dir: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start(name: &str, files: &[(&str, &str)]) -> Self {
        let dir =
            std::env::temp_dir().join(format!("galdeck-motion-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, body) in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
        }
        let (workspace, diagnostics) = Workspace::load(&dir);
        let workspace = workspace.unwrap_or_else(|| panic!("config should load: {diagnostics:#?}"));
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == galdeck_model::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "fixture is broken: {errors:#?}");

        let shutdown = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let deadline = Arc::new(DeadlineCell::new());
        let (waker, wake_rx) = wake_channel();
        let (paint_tx, paint_rx) = sync_channel(256);
        let (device_tx, device_rx) = sync_channel(256);
        let (control_tx, control_rx) = channel();
        let control = ControlSender::new(control_tx, waker.clone());
        let (widget_host, widget_rx) = WidgetHost::new(waker.clone());
        let (plugin_host, plugin_rx) = PluginHost::discover(&dir, waker.clone());
        let (deck, handle) = FakeDeck::new();
        let preview = Preview::new();
        let io = IoThread::with_deck(
            Box::new(deck),
            paint_rx,
            device_tx,
            Arc::clone(&deadline),
            Arc::clone(&clock),
            waker,
            Arc::clone(&shutdown),
            Arc::clone(&parked),
        );
        let io_thread = std::thread::spawn(move || io.run());
        let mut engine = Engine::new(
            dir.clone(),
            workspace,
            EngineParts {
                control_rx,
                device_rx,
                paint_tx,
                widget_rx,
                wake: wake_rx,
                deadline,
                clock,
                shutdown: Arc::clone(&shutdown),
                preview: preview.clone(),
                widget_host,
                plugin_host,
                plugin_rx,
                parked: Arc::clone(&parked),
                zone_paint: true,
                calibration_path: Some(
                    std::env::temp_dir().join("galdeck-no-such-calibration.conf"),
                ),
            },
        )
        .expect("engine builds");
        let core = std::thread::spawn(move || engine.run());
        let harness = Self {
            control,
            deck: handle,
            preview,
            dir,
            shutdown,
            threads: vec![io_thread, core],
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !harness.status().connected {
            assert!(Instant::now() < deadline, "the fake deck never connected");
            std::thread::sleep(Duration::from_millis(10));
        }
        harness
    }

    fn status(&self) -> Status {
        let (reply, rx) = channel();
        self.control
            .send(ControlMsg {
                request: Request::Status,
                reply,
            })
            .expect("engine listening");
        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("engine answers")
        {
            Response::Status(status) => status,
            other => panic!("not a status: {other:?}"),
        }
    }

    /// How far the lower `share` of key `key`'s picture leans towards
    /// `channel` (0 red, 1 green, 2 blue): that channel less the average of
    /// the other two, averaged over the pixels.
    fn lean(&self, key: u8, channel: usize, share: f64) -> Option<f64> {
        let jpeg = self.preview.key(key)?;
        let picture = image::load_from_memory(&jpeg).ok()?.to_rgb8();
        let from = (f64::from(picture.height()) * (1.0 - share)) as u32;
        let (mut sum, mut count) = (0.0, 0.0);
        for (_, y, p) in picture.enumerate_pixels() {
            if y < from {
                continue;
            }
            let others: f64 = (0..3)
                .filter(|c| *c != channel)
                .map(|c| f64::from(p[c]))
                .sum();
            sum += f64::from(p[channel]) - others / 2.0;
            count += 1.0;
        }
        Some(sum / count)
    }

    /// Watch `sample` for `over`, every few milliseconds, and give back
    /// every value it had.
    fn watch<T>(&self, over: Duration, sample: impl Fn() -> Option<T>) -> Vec<T> {
        let deadline = Instant::now() + over;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            seen.extend(sample());
            std::thread::sleep(Duration::from_millis(15));
        }
        seen
    }

    fn ring(&self, encoder: u8) -> [Rgb; 4] {
        *self.deck.surface().ring(encoder)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let (reply, _rx) = channel();
        let _ = self.control.send(ControlMsg {
            request: Request::Ping,
            reply,
        });
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const GLOBAL: &str = r#"
version = 2
brightness = 60
profile = "p"

# Knob 0 turns in modes whose rings show nothing but the mode's colour;
# knob 1 is set to nothing at all.
[[encoders]]
encoder = 0
modes = ["scroll", "zoom"]
"#;

const THEME: &str = r##"
[style]
key_bg = "#202020"
ring = "#000000"

[motion]
press = { kind = "flash", color = "#0000ff", ms = 600 }
alarm = { kind = "blink", period_ms = 400 }
rings = { kind = "blink", period_ms = 400, to = "#ff0000" }
"##;

const PROFILE: &str = r##"
theme = "t"

[[pages]]
id = "one"

[[pages.keys]]
key = 0
label = "Go"
exec = "true"

[[pages.keys]]
key = 1
widget = { kind = "command", command = "echo 95", view = "bar", warn = 50, critical = 90, interval_ms = 300 }
"##;

fn start(name: &str) -> Harness {
    Harness::start(
        name,
        &[
            ("galdeck.toml", GLOBAL),
            ("themes/t.toml", THEME),
            ("profiles/p.toml", PROFILE),
        ],
    )
}

#[test]
fn a_press_flashes_its_key_and_the_flash_fades() {
    let harness = start("press");
    let blue = || harness.lean(0, 2, 1.0);
    let before = harness.watch(Duration::from_millis(200), blue);
    let resting = before.iter().copied().fold(f64::MIN, f64::max);
    assert!(resting < 10.0, "the key is grey to begin with: {resting}");

    harness.deck.press(Event::KeyDown(0));
    harness.deck.press(Event::KeyUp(0));
    let during = harness.watch(Duration::from_millis(300), blue);
    let flash = during.iter().copied().fold(f64::MIN, f64::max);
    assert!(flash > 40.0, "the key flashed towards blue: {flash}");

    // Faded by now, and nothing left over.
    std::thread::sleep(Duration::from_millis(700));
    let after = harness.watch(Duration::from_millis(200), blue);
    let settled = after.iter().copied().fold(f64::MIN, f64::max);
    assert!(settled < resting + 5.0, "the flash faded: {settled}");
}

#[test]
fn an_alarm_blinks_between_its_colour_and_the_alarms() {
    let harness = start("alarm");
    // The bar is along the bottom of the key; a blink makes its fill red
    // for one frame and its usual white for the next.
    let red = || harness.lean(1, 0, 0.3);
    let seen = harness.watch(Duration::from_millis(2500), red);
    let high = seen.iter().copied().fold(f64::MIN, f64::max);
    let low = seen.iter().copied().fold(f64::MAX, f64::min);
    assert!(high > 20.0, "the bar showed the alarm's red: {high}");
    assert!(low < 5.0, "and its usual colour between: {low}");
}

#[test]
fn a_theme_ring_animation_moves_a_plain_knob_and_not_one_in_modes() {
    let harness = start("rings");
    let red = |colors: [Rgb; 4]| colors.iter().any(|c| c.r > 200 && c.g < 60 && c.b < 60);
    let plain = harness.watch(Duration::from_millis(1500), || Some(red(harness.ring(1))));
    assert!(plain.contains(&true), "the plain knob's ring reached red");
    assert!(plain.contains(&false), "and came back from it");
    let modes = harness.watch(Duration::from_millis(800), || Some(red(harness.ring(0))));
    assert!(
        !modes.contains(&true),
        "a knob in modes keeps its mode's colour"
    );
}
