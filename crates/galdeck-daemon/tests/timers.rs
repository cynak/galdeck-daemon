//! Timers and stopwatches on keys, driven through a fake deck.
//!
//! The state machine itself is tested in `countdown.rs` over made-up ticks;
//! these check the engine around it: that a timer keeps counting while its
//! page is not showing, survives a reload, refuses a hold while it runs, and
//! says so when it finishes out of sight. Timers here are a second long, so
//! the one test that waits for one to finish waits about that. Nothing in
//! these tests touches the machine's sound, media players or input devices.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use galdeck::Event;
use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine, EngineParts};
use galdeck_daemon::io::IoThread;
use galdeck_daemon::plugins::PluginHost;
use galdeck_daemon::preview::Preview;
use galdeck_daemon::widgets::WidgetHost;
use galdeck_device::{FakeDeck, FakeDeckHandle};
use galdeck_ipc::{KeyInfo, Request, Response, Status, TimerInfo};
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
            std::env::temp_dir().join(format!("galdeck-timers-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (file, body) in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body.replace("{dir}", &dir.display().to_string())).unwrap();
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
        assert!(
            harness.until(|s| s.connected),
            "the fake deck never connected"
        );
        harness
    }

    fn request(&self, request: Request) -> Response {
        let (reply, rx) = channel();
        self.control
            .send(ControlMsg { request, reply })
            .expect("engine listening");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("engine answers")
    }

    fn status(&self) -> Status {
        match self.request(Request::Status) {
            Response::Status(status) => status,
            other => panic!("not a status: {other:?}"),
        }
    }

    /// Wait for the status to satisfy `test`.
    fn until(&self, test: impl Fn(&Status) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if test(&self.status()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Key `key` as the page showing describes it.
    fn key(&self, key: u8) -> KeyInfo {
        let Response::Layout(layout) = self.request(Request::GetLayout) else {
            panic!("no layout");
        };
        layout
            .keys
            .into_iter()
            .find(|info| info.key == key)
            .unwrap_or_else(|| panic!("no key {key} on the page"))
    }

    /// The timer on key `key` of the page showing.
    fn timer(&self, key: u8) -> TimerInfo {
        self.key(key)
            .timer
            .unwrap_or_else(|| panic!("key {key} has no timer"))
    }

    /// Wait for the timer on `key` to satisfy `test`.
    fn until_timer(&self, key: u8, test: impl Fn(&TimerInfo) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if test(&self.timer(key)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// A press and release, well short of a hold.
    fn tap(&self, key: u8) {
        self.deck.press(Event::KeyDown(key));
        self.deck.press(Event::KeyUp(key));
    }

    /// A press held well past the hold threshold, then released.
    fn hold(&self, key: u8) {
        self.deck.press(Event::KeyDown(key));
        std::thread::sleep(Duration::from_millis(700));
        self.deck.press(Event::KeyUp(key));
    }

    fn switch_page(&self, name: &str) {
        assert!(matches!(
            self.request(Request::SwitchPage { name: name.into() }),
            Response::Ok
        ));
    }

    /// How far key `key`'s picture leans towards red, as the average of red
    /// less the other two: near nothing for a grey key with white text, well
    /// above it for one tinted towards the theme's `@critical`.
    fn redness(&self, key: u8) -> f64 {
        let jpeg = self.preview.key(key).expect("the key has a picture");
        let picture = image::load_from_memory(&jpeg)
            .expect("the key's picture is a JPEG")
            .to_rgb8();
        let sum: f64 = picture
            .pixels()
            .map(|p| f64::from(p[0]) - (f64::from(p[1]) + f64::from(p[2])) / 2.0)
            .sum();
        sum / f64::from(picture.width() * picture.height())
    }

    /// Whether a file the fixture's commands write has appeared.
    fn wrote(&self, name: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.dir.join(name).exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Only to wake the loop so it sees the flag: a stopping engine does
        // not answer, so nothing waits for a reply.
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

const GLOBAL: &str = "version = 2\nprofile = \"p\"\n";

/// Two pages, with a timer of `duration` on key 0 of the first.
fn timer_pages(duration: &str, extra: &str) -> String {
    format!(
        r#"
[[pages]]
id = "one"
[[pages.keys]]
key = 0
label = "Tea"
widget = {{ kind = "timer", duration = "{duration}"{extra} }}

[[pages]]
id = "two"
[[pages.keys]]
key = 0
label = "elsewhere"
"#
    )
}

#[test]
fn a_timer_keeps_running_across_a_page_switch_and_a_reload() {
    let harness = Harness::start(
        "survives",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &timer_pages("25m", "")),
        ],
    );
    let key = harness.key(0);
    // The widget gives the key both gestures, and the editor is told they
    // are the widget's rather than the key's own.
    let tap = key.tap.expect("an implicit tap");
    let hold = key.hold.expect("an implicit hold");
    assert_eq!(
        (tap.kind.as_str(), tap.action.as_deref()),
        ("implicit", Some("timer_toggle"))
    );
    assert_eq!(
        (hold.kind.as_str(), hold.action.as_deref()),
        ("implicit", Some("timer_reset"))
    );
    let fresh = harness.timer(0);
    assert_eq!(fresh.state, "stopped");
    assert_eq!(fresh.remaining_ms, Some(25 * 60 * 1000));

    harness.tap(0);
    assert!(harness.until_timer(0, |t| t.state == "running"));

    harness.switch_page("two");
    assert!(harness.key(0).timer.is_none(), "page two has no timer");
    std::thread::sleep(Duration::from_millis(300));
    harness.switch_page("one");
    let back = harness.timer(0);
    assert_eq!(back.state, "running", "{back:?}");
    assert!(back.elapsed_ms >= 300, "{back:?}");

    // A save that leaves the timer as it was keeps it counting.
    std::fs::write(
        harness.dir.join("profiles/p.toml"),
        timer_pages("25m", "").replace("elsewhere", "somewhere else"),
    )
    .unwrap();
    assert!(matches!(harness.request(Request::Reload), Response::Ok));
    let reloaded = harness.timer(0);
    assert_eq!(reloaded.state, "running", "{reloaded:?}");
    assert!(reloaded.elapsed_ms >= back.elapsed_ms, "{reloaded:?}");

    // One that changes its length is a different timer, and starts over.
    std::fs::write(harness.dir.join("profiles/p.toml"), timer_pages("10m", "")).unwrap();
    assert!(matches!(harness.request(Request::Reload), Response::Ok));
    let changed = harness.timer(0);
    assert_eq!(changed.state, "stopped", "{changed:?}");
    assert_eq!(changed.remaining_ms, Some(10 * 60 * 1000));
}

#[test]
fn a_running_timer_refuses_a_hold_and_a_paused_one_resets() {
    let harness = Harness::start(
        "hold",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &timer_pages("25m", "")),
        ],
    );
    harness.tap(0);
    assert!(harness.until_timer(0, |t| t.state == "running"));

    // A slow tap reaches a hold; it must not wipe out a running timer.
    harness.hold(0);
    std::thread::sleep(Duration::from_millis(100));
    let held = harness.timer(0);
    assert_eq!(held.state, "running", "{held:?}");

    harness.tap(0);
    assert!(harness.until_timer(0, |t| t.state == "paused"));
    harness.hold(0);
    assert!(harness.until_timer(0, |t| t.state == "stopped"
        && t.elapsed_ms == 0
        && t.remaining_ms == Some(25 * 60 * 1000)));
}

#[test]
fn a_finished_timer_is_put_back_to_its_start_by_a_tap() {
    let harness = Harness::start(
        "ack",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &timer_pages("1s", "")),
        ],
    );
    harness.tap(0);
    assert!(harness.until_timer(0, |t| t.state == "done"));
    assert_eq!(harness.timer(0).remaining_ms, Some(0));

    harness.tap(0);
    assert!(harness.until_timer(0, |t| t.state == "stopped"));
    assert_eq!(harness.timer(0).remaining_ms, Some(1000));
}

#[test]
fn a_timer_that_finishes_in_sight_flashes_until_it_is_tapped() {
    let harness = Harness::start(
        "flash",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &timer_pages("1s", "")),
        ],
    );
    let plain = harness.redness(0);
    harness.tap(0);
    assert!(harness.until_timer(0, |t| t.state == "done"));

    // Nothing but the flash repaints a finished timer's key, so seeing both
    // halves means the flash is running.
    let (mut lit, mut dark) = (false, false);
    let deadline = Instant::now() + Duration::from_millis(1500);
    while !(lit && dark) && Instant::now() < deadline {
        let redness = harness.redness(0);
        lit |= redness > plain + 25.0;
        dark |= redness < plain + 8.0;
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(lit && dark, "lit {lit}, dark {dark}: the key did not flash");

    // The tap acknowledges it, and nothing lights it again.
    harness.tap(0);
    assert!(harness.until_timer(0, |t| t.state == "stopped"));
    let deadline = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < deadline {
        let redness = harness.redness(0);
        assert!(redness < plain + 8.0, "still tinted after a tap: {redness}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_timer_that_finishes_out_of_sight_says_so_and_runs_its_on_done() {
    let harness = Harness::start(
        "hidden",
        &[
            ("galdeck.toml", GLOBAL),
            (
                "profiles/p.toml",
                &timer_pages("1s", ", on_done = \"touch '{dir}/done'\""),
            ),
        ],
    );
    let events = harness.preview.subscribe();
    harness.tap(0);
    assert!(harness.until_timer(0, |t| t.state == "running"));
    harness.switch_page("two");

    assert!(
        harness.wrote("done", Duration::from_secs(5)),
        "on_done never ran"
    );
    // Other events come and go on the way: key presses, the page switch.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut told = None;
    while told.is_none() && Instant::now() < deadline {
        if let Ok(galdeck_ipc::Event::TimerDone { profile, page, key }) =
            events.recv_timeout(Duration::from_millis(100))
        {
            told = Some((profile, page, key));
        }
    }
    let (profile, page, key) = told.expect("no TimerDone was published");
    assert_eq!((profile.as_str(), page.as_str(), key), ("p", "one", 0));
    assert_eq!(harness.status().page, "two", "finishing switched no page");

    // Back on its page, it is waiting to be acknowledged.
    harness.switch_page("one");
    assert_eq!(harness.timer(0).state, "done");
}

#[test]
fn the_editor_cannot_run_what_only_a_key_or_a_knob_can() {
    let harness = Harness::start(
        "editor",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &timer_pages("25m", "")),
        ],
    );
    for action in ["timer_toggle", "timer_reset", "next_mode", "next_app"] {
        let fields = [(
            "action".to_string(),
            galdeck_model::Value::String(action.into()),
        )]
        .into_iter()
        .collect();
        assert!(
            matches!(
                harness.request(Request::RunAction { fields }),
                Response::Error { .. }
            ),
            "{action} was run from the editor"
        );
    }
    assert_eq!(harness.timer(0).state, "stopped");
}

#[test]
fn a_stopwatch_counts_up_and_resets_while_running() {
    let harness = Harness::start(
        "stopwatch",
        &[
            ("galdeck.toml", GLOBAL),
            (
                "profiles/p.toml",
                "[[pages]]\nid = \"one\"\n[[pages.keys]]\nkey = 2\nwidget = { kind = \"stopwatch\" }\n",
            ),
        ],
    );
    let fresh = harness.timer(2);
    assert_eq!(
        (fresh.state.as_str(), fresh.remaining_ms),
        ("stopped", None)
    );
    harness.tap(2);
    assert!(harness.until_timer(2, |t| t.state == "running" && t.elapsed_ms >= 200));
    // Starting a count over is what a stopwatch's hold is for.
    harness.hold(2);
    assert!(harness.until_timer(2, |t| t.state == "stopped" && t.elapsed_ms == 0));
}
