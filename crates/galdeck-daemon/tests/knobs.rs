//! Knobs: presets, layers, holds, and spins, driven through a fake deck.
//!
//! Only built-ins the engine does itself are exercised here -- pages,
//! profiles and the deck's brightness -- plus shell commands that write to a
//! temporary file. Nothing in these tests touches the machine's sound, media
//! players or input devices.

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
use galdeck_ipc::{Request, Response, Status};
use galdeck_model::Workspace;

struct Harness {
    control: ControlSender,
    deck: FakeDeckHandle,
    dir: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start(name: &str, files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("galdeck-knobs-{name}-{}", std::process::id()));
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
                preview: Preview::new(),
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

    fn turn(&self, encoder: u8, delta: i8) {
        self.deck.press(Event::EncoderRotate(encoder, delta));
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

const THREE_PAGES: &str = r#"
[[pages]]
id = "one"
[[pages]]
id = "two"
[[pages]]
id = "three"
"#;

#[test]
fn a_pages_knob_set_once_turns_through_every_page_and_wraps() {
    let harness = Harness::start(
        "pages",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\n[[encoders]]\nencoder = 0\npreset = \"pages\"\n",
            ),
            ("profiles/p.toml", THREE_PAGES),
        ],
    );
    assert_eq!(harness.status().page, "one");
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "two"));
    // A fast spin is one report with a bigger delta: two pages at once.
    harness.turn(0, 2);
    assert!(
        harness.until(|s| s.page == "one"),
        "three pages on from two wraps to one"
    );
    harness.turn(0, -1);
    assert!(
        harness.until(|s| s.page == "three"),
        "back from the first wraps to the last"
    );

    // Pressing goes to the home page, which is the first.
    harness.deck.press(Event::EncoderDown(0));
    harness.deck.press(Event::EncoderUp(0));
    assert!(harness.until(|s| s.page == "one"));
}

#[test]
fn turning_through_pages_leaves_no_trail_for_back() {
    let harness = Harness::start(
        "trail",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\n[[encoders]]\nencoder = 0\npreset = \"pages\"\n",
            ),
            (
                "profiles/p.toml",
                r#"
[[pages]]
id = "one"
[[pages]]
id = "two"
[[pages.keys]]
key = 0
back = true
"#,
            ),
        ],
    );
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "two"));
    let Response::Layout(layout) = harness.request(Request::GetLayout) else {
        panic!("no layout");
    };
    // A dial is not a series of visits: nothing to go back to.
    assert!(!layout.can_go_back);
}

#[test]
fn a_spin_through_pages_keeps_going_on_a_page_that_binds_the_knob_otherwise() {
    let harness = Harness::start(
        "latch",
        &[
            ("galdeck.toml", "version = 2\nprofile = \"p\"\n"),
            (
                "profiles/p.toml",
                r#"
[[pages]]
id = "one"
[[pages.encoders]]
encoder = 0
preset = "pages"

[[pages]]
id = "two"
[[pages.encoders]]
encoder = 0
cw = "touch '{dir}/turned'"
ccw = "touch '{dir}/turned'"

[[pages]]
id = "three"
"#,
            ),
        ],
    );
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "two"));
    // Still mid-spin: this detent is the spin's, not page two's command.
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "three"));
    assert!(!harness.wrote("turned", Duration::from_millis(200)));
}

#[test]
fn after_a_pause_the_page_s_own_knob_takes_over() {
    let harness = Harness::start(
        "unlatch",
        &[
            ("galdeck.toml", "version = 2\nprofile = \"p\"\n"),
            (
                "profiles/p.toml",
                r#"
[[pages]]
id = "one"
[[pages.encoders]]
encoder = 0
preset = "pages"

[[pages]]
id = "two"
[[pages.encoders]]
encoder = 0
cw = "touch '{dir}/turned'"
"#,
            ),
        ],
    );
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "two"));
    std::thread::sleep(Duration::from_millis(800));
    harness.turn(0, 1);
    assert!(harness.wrote("turned", Duration::from_secs(3)));
    assert_eq!(harness.status().page, "two");
}

#[test]
fn a_knob_with_a_hold_presses_on_release_and_holds_after_a_while() {
    let knob = r#"
[[pages]]
id = "one"
[[pages.encoders]]
encoder = 1
press = "touch '{dir}/pressed'"
hold = "touch '{dir}/held'"
"#;
    let harness = Harness::start(
        "hold",
        &[
            ("galdeck.toml", "version = 2\nprofile = \"p\"\n"),
            ("profiles/p.toml", knob),
        ],
    );
    // A click: pressed on the way up, and not held.
    harness.deck.press(Event::EncoderDown(1));
    assert!(
        !harness.wrote("pressed", Duration::from_millis(100)),
        "a press must wait for the release"
    );
    harness.deck.press(Event::EncoderUp(1));
    assert!(harness.wrote("pressed", Duration::from_secs(3)));
    assert!(!harness.wrote("held", Duration::from_millis(600)));

    // A hold: held, and not also pressed.
    std::fs::remove_file(harness.dir.join("pressed")).unwrap();
    harness.deck.press(Event::EncoderDown(1));
    assert!(harness.wrote("held", Duration::from_secs(3)));
    harness.deck.press(Event::EncoderUp(1));
    assert!(!harness.wrote("pressed", Duration::from_millis(400)));
}

#[test]
fn a_press_that_changes_page_is_not_answered_again_by_the_page_it_lands_on() {
    let pages = r#"
[[pages]]
id = "one"
[[pages.encoders]]
encoder = 1
press = "touch '{dir}/one_pressed'"
hold = "touch '{dir}/one_held'"

[[pages]]
id = "two"
[[pages.encoders]]
encoder = 1
press = { action = "home_page" }
"#;
    let harness = Harness::start(
        "cross-page",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\n[[encoders]]\nencoder = 0\npreset = \"pages\"\n",
            ),
            ("profiles/p.toml", pages),
        ],
    );
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "two"));

    // Page two has no hold, so its press goes home on the way down. Page
    // one binds a press and a hold of its own, and neither is this click's.
    harness.deck.press(Event::EncoderDown(1));
    assert!(harness.until(|s| s.page == "one"));
    std::thread::sleep(Duration::from_millis(100));
    harness.deck.press(Event::EncoderUp(1));
    assert!(!harness.wrote("one_pressed", Duration::from_millis(400)));

    // Held down across the change, page one's hold is not this press's
    // either.
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "two"));
    harness.deck.press(Event::EncoderDown(1));
    assert!(harness.until(|s| s.page == "one"));
    assert!(!harness.wrote("one_held", Duration::from_millis(900)));
    harness.deck.press(Event::EncoderUp(1));
    assert!(!harness.wrote("one_pressed", Duration::from_millis(300)));

    // On page one itself, the click is page one's.
    harness.deck.press(Event::EncoderDown(1));
    harness.deck.press(Event::EncoderUp(1));
    assert!(harness.wrote("one_pressed", Duration::from_secs(3)));
}

#[test]
fn pressing_while_turning_is_a_grip_not_a_click() {
    let knob = r#"
[[pages]]
id = "one"
[[pages.encoders]]
encoder = 1
press = "touch '{dir}/pressed'"
hold = "touch '{dir}/held'"
cw = "true"
"#;
    let harness = Harness::start(
        "grip",
        &[
            ("galdeck.toml", "version = 2\nprofile = \"p\"\n"),
            ("profiles/p.toml", knob),
        ],
    );
    harness.deck.press(Event::EncoderDown(1));
    harness.turn(1, 1);
    std::thread::sleep(Duration::from_millis(600));
    harness.deck.press(Event::EncoderUp(1));
    assert!(!harness.wrote("pressed", Duration::from_millis(300)));
    assert!(!harness.wrote("held", Duration::from_millis(100)));
}

#[test]
fn a_page_that_sets_only_press_keeps_the_inherited_turn() {
    let harness = Harness::start(
        "layers",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\n[[encoders]]\nencoder = 0\npreset = \"deck_brightness\"\nstep = 10\n",
            ),
            (
                "profiles/p.toml",
                r#"
[[pages]]
id = "one"
[[pages.encoders]]
encoder = 0
press = "touch '{dir}/pressed'"
"#,
            ),
        ],
    );
    let before = harness.status().brightness;
    harness.turn(0, -1);
    assert!(
        harness.until(|s| s.brightness + 10 == before),
        "the global turn still dims"
    );
    harness.deck.press(Event::EncoderDown(0));
    harness.deck.press(Event::EncoderUp(0));
    assert!(harness.wrote("pressed", Duration::from_secs(3)));

    let Response::Layout(layout) = harness.request(Request::GetLayout) else {
        panic!("no layout");
    };
    let knob = layout.encoders.iter().find(|e| e.encoder == 0).unwrap();
    assert_eq!(
        knob.resolved.cw.as_ref().unwrap().origin.as_deref(),
        Some("global")
    );
    assert_eq!(
        knob.resolved.press.as_ref().unwrap().origin.as_deref(),
        Some("page")
    );
    assert_eq!(knob.ring_shows.as_deref(), Some("deck_brightness"));
    let layers: Vec<&str> = knob.layers.iter().map(|l| l.layer.as_str()).collect();
    assert_eq!(layers, ["global", "page"]);
}

#[test]
fn the_deck_is_never_turned_all_the_way_dark() {
    let harness = Harness::start(
        "dark",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\nbrightness = 20\n[[encoders]]\nencoder = 0\npreset = \"deck_brightness\"\nstep = 25\n",
            ),
            ("profiles/p.toml", "[[pages]]\nid = \"one\"\n"),
        ],
    );
    harness.turn(0, -8);
    assert!(
        harness.until(|s| s.brightness == 5),
        "got {}",
        harness.status().brightness
    );
}

#[test]
fn brightness_set_with_a_knob_survives_a_save_until_the_file_changes_it() {
    let config = |brightness: u8| {
        format!(
            "version = 2\nprofile = \"p\"\nbrightness = {brightness}\n[[encoders]]\nencoder = 0\npreset = \"deck_brightness\"\nstep = 10\n"
        )
    };
    let harness = Harness::start(
        "brightness-reload",
        &[
            ("galdeck.toml", &config(60)),
            (
                "profiles/p.toml",
                "[[pages]]\nid = \"one\"\n[[pages.keys]]\nkey = 0\nlabel = \"a\"\n",
            ),
        ],
    );
    harness.turn(0, -2);
    assert!(harness.until(|s| s.brightness == 40));

    // A save that leaves brightness alone, as any edit in the editor does.
    std::fs::write(
        harness.dir.join("profiles/p.toml"),
        "[[pages]]\nid = \"one\"\n[[pages.keys]]\nkey = 0\nlabel = \"b\"\n",
    )
    .unwrap();
    assert!(matches!(harness.request(Request::Reload), Response::Ok));
    assert_eq!(harness.status().brightness, 40);

    // A new value in the file is a new wish.
    std::fs::write(harness.dir.join("galdeck.toml"), config(70)).unwrap();
    assert!(matches!(harness.request(Request::Reload), Response::Ok));
    assert_eq!(harness.status().brightness, 70);
}

#[test]
fn a_profiles_knob_turns_through_profiles_in_order() {
    let harness = Harness::start(
        "profiles",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"a\"\n[[encoders]]\nencoder = 1\npreset = \"profiles\"\n",
            ),
            ("profiles/a.toml", "[[pages]]\nid = \"main\"\n"),
            ("profiles/b.toml", "[[pages]]\nid = \"main\"\n"),
            ("profiles/c.toml", "[[pages]]\nid = \"main\"\n"),
        ],
    );
    harness.turn(1, 1);
    assert!(harness.until(|s| s.profile == "b"));
    harness.turn(1, -2);
    assert!(
        harness.until(|s| s.profile == "c"),
        "back two from b wraps to c"
    );
    // Press goes to the start profile.
    harness.deck.press(Event::EncoderDown(1));
    harness.deck.press(Event::EncoderUp(1));
    assert!(harness.until(|s| s.profile == "a"));
}

#[test]
fn a_shell_command_on_a_knob_still_runs_once_per_detent() {
    let knob = r#"
[[pages]]
id = "one"
[[pages.encoders]]
encoder = 0
cw = "echo \"$GALDECK_DELTA\" >> '{dir}/detents'"
"#;
    let harness = Harness::start(
        "detents",
        &[
            ("galdeck.toml", "version = 2\nprofile = \"p\"\n"),
            ("profiles/p.toml", knob),
        ],
    );
    harness.turn(0, 3);
    let path = harness.dir.join("detents");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut lines = 0;
    while Instant::now() < deadline {
        lines = std::fs::read_to_string(&path)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        if lines == 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lines, 3);
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .all(|l| l == "3"));
}

#[test]
fn the_catalog_lists_what_the_model_knows() {
    let harness = Harness::start(
        "catalog",
        &[
            ("galdeck.toml", "version = 2\nprofile = \"p\"\n"),
            ("profiles/p.toml", "[[pages]]\nid = \"one\"\n"),
        ],
    );
    let Response::Catalog(catalog) = harness.request(Request::Catalog) else {
        panic!("no catalog");
    };
    assert!(catalog
        .presets
        .iter()
        .any(|p| p.name == "volume" && p.ring_shows.as_deref() == Some("output_level")));
    let ptt = catalog
        .built_ins
        .iter()
        .find(|b| b.name == "push_to_talk")
        .unwrap();
    assert!(ptt.keys_only);
    assert!(catalog.keys.iter().any(|k| k.name == "kp_enter"));
    assert!(!catalog.keys.iter().any(|k| k.name == "sysrq"));
}

#[test]
fn the_editor_cannot_hold_a_microphone_open() {
    let harness = Harness::start(
        "ptt",
        &[
            ("galdeck.toml", "version = 2\nprofile = \"p\"\n"),
            ("profiles/p.toml", "[[pages]]\nid = \"one\"\n"),
        ],
    );
    let fields = [(
        "action".to_string(),
        galdeck_model::Value::String("push_to_talk".into()),
    )]
    .into_iter()
    .collect();
    assert!(matches!(
        harness.request(Request::RunAction { fields }),
        Response::Error { .. }
    ));
    // And a nonsense action is refused rather than guessed at.
    let fields = [(
        "action".to_string(),
        galdeck_model::Value::String("launch_missiles".into()),
    )]
    .into_iter()
    .collect();
    assert!(matches!(
        harness.request(Request::RunAction { fields }),
        Response::Error { .. }
    ));
}

#[test]
fn the_status_says_what_the_machine_allows() {
    let harness = Harness::start(
        "caps",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\nvirtual_input = false\n",
            ),
            ("profiles/p.toml", "[[pages]]\nid = \"one\"\n"),
        ],
    );
    let status = harness.status();
    assert_eq!(status.capabilities.virtual_input, "off");
    assert!(["wpctl", "pactl", "missing"].contains(&status.capabilities.audio.as_str()));
}

#[test]
fn the_layout_says_what_each_layer_sets_and_lists_knobs_set_nowhere() {
    let harness = Harness::start(
        "layer-look",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\n[[encoders]]\nencoder = 0\npreset = \"volume\"\nstyle = { ring = \"#ff0000\" }\nanimation = { kind = \"pulse\" }\n",
            ),
            (
                "profiles/p.toml",
                "[[pages]]\nid = \"one\"\n[[pages.encoders]]\nencoder = 0\npress = \"true\"\n",
            ),
        ],
    );
    let Response::Layout(layout) = harness.request(Request::GetLayout) else {
        panic!("no layout");
    };
    assert_eq!(layout.encoders.len(), 2, "every knob is listed");

    let set = layout.encoders.iter().find(|e| e.encoder == 0).unwrap();
    let global = set.layers.iter().find(|l| l.layer == "global").unwrap();
    assert_eq!(global.ring.as_deref(), Some("#ff0000"));
    assert_eq!(
        global.animation.as_ref().map(|a| a.kind.as_str()),
        Some("pulse")
    );
    // The page inherits both, and says so by not claiming them.
    let page = set.layers.iter().find(|l| l.layer == "page").unwrap();
    assert!(page.ring.is_none() && page.animation.is_none(), "{page:#?}");

    // Set nowhere, but with the colour the theme gives it rather than none.
    let unset = layout.encoders.iter().find(|e| e.encoder == 1).unwrap();
    assert!(unset.layers.is_empty());
    assert!(unset.index.is_none());
    assert_eq!(unset.base_ring.as_deref(), Some(unset.ring.as_str()));
}
