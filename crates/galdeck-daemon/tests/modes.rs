//! Knobs with modes: holding one switches what turning it does, driven
//! through a fake deck.
//!
//! The modes here are only ones the engine does itself -- pages, profiles and
//! the deck's brightness -- plus shell commands that write to a temporary
//! file. Nothing in these tests touches the machine's sound, media players or
//! input devices; the one that asks for the outputs and apps only reads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver};
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
use galdeck_ipc::{EncoderInfo, Patch, Request, Response, Status, Value};
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
        let dir = std::env::temp_dir().join(format!("galdeck-modes-{name}-{}", std::process::id()));
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

    /// Knob `encoder` as the page showing describes it.
    fn knob(&self, encoder: u8) -> EncoderInfo {
        let Response::Layout(layout) = self.request(Request::GetLayout) else {
            panic!("no layout");
        };
        layout
            .encoders
            .into_iter()
            .find(|info| info.encoder == encoder)
            .unwrap_or_else(|| panic!("no knob {encoder}"))
    }

    /// Wait for knob `encoder` to be in `mode`.
    fn until_mode(&self, encoder: u8, mode: Option<usize>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.knob(encoder).mode == mode {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Wait for ring `encoder` to show `colors`.
    fn until_ring(&self, encoder: u8, colors: [Rgb; 4]) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.preview.frame().rings[encoder as usize] == colors {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn turn(&self, encoder: u8, delta: i8) {
        self.deck.press(Event::EncoderRotate(encoder, delta));
    }

    /// Hold a knob down until its mode changes, then let it go.
    fn hold_for_mode(&self, encoder: u8, mode: usize) {
        self.deck.press(Event::EncoderDown(encoder));
        assert!(
            self.until_mode(encoder, Some(mode)),
            "holding knob {encoder} never reached mode {mode}"
        );
        self.deck.press(Event::EncoderUp(encoder));
    }

    fn switch_page(&self, name: &str) {
        assert!(matches!(
            self.request(Request::SwitchPage { name: name.into() }),
            Response::Ok
        ));
    }

    fn reload_with(&self, file: &str, body: &str) {
        std::fs::write(self.dir.join(file), body).unwrap();
        assert!(matches!(self.request(Request::Reload), Response::Ok));
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

/// The left knob dims and brightens the deck, or turns through pages once
/// held: set once, in galdeck.toml.
const BRIGHTNESS_AND_PAGES: &str = r##"
version = 2
profile = "p"
brightness = 50
[[encoders]]
encoder = 0
modes = [{ preset = "deck_brightness", step = 10 }, { preset = "pages", ring = "#00ff00" }]
"##;

/// Three pages, and on the second the left knob goes through profiles
/// instead: a page that sets its own preset shadows the modes beneath it.
const PAGES: &str = r#"
[[pages]]
id = "one"
[[pages.keys]]
key = 0
label = "a"

[[pages]]
id = "two"
[[pages.encoders]]
encoder = 0
preset = "profiles"

[[pages]]
id = "three"
"#;

/// A ring resting at `rest` with segment `lit` showing a position.
fn position(rest: Rgb, lit: usize) -> [Rgb; 4] {
    let mut colors = [rest; 4];
    colors[lit] = rest.lerp(Rgb::WHITE, 0.7);
    colors
}

/// The next `ModeChanged`, if one comes.
fn next_mode_change(events: &Receiver<galdeck_ipc::Event>) -> Option<(u8, usize)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(galdeck_ipc::Event::ModeChanged { encoder, mode }) => return Some((encoder, mode)),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn holding_a_knob_with_modes_switches_what_turning_it_does() {
    let harness = Harness::start(
        "switch",
        &[
            ("galdeck.toml", BRIGHTNESS_AND_PAGES),
            ("profiles/p.toml", PAGES),
        ],
    );
    let events = harness.preview.subscribe();
    let knob = harness.knob(0);
    assert_eq!(knob.mode, Some(0));
    assert_eq!(knob.turn_preset.as_deref(), Some("deck_brightness"));
    let hold = knob
        .resolved
        .hold
        .as_ref()
        .expect("a hold that switches mode");
    assert_eq!(hold.action.as_deref(), Some("next_mode"));
    assert_eq!(hold.origin.as_deref(), Some("global"));
    // Nobody wrote it: the modes brought it.
    assert_eq!(hold.kind, "implicit");

    // The first mode dims.
    harness.turn(0, -1);
    assert!(harness.until(|s| s.brightness == 40));

    // Longer than a key's hold is not yet long enough: a knob with modes
    // always has a hold, and a slow press is not a wish to switch.
    // Measured from before the press, so a slow machine can only make it
    // look longer: a key's 400 ms hold would still fail it.
    let pressed = Instant::now();
    harness.deck.press(Event::EncoderDown(0));
    assert!(harness.until_mode(0, Some(1)));
    assert!(
        pressed.elapsed() >= Duration::from_millis(600),
        "switched after {:?}, before 650 ms",
        pressed.elapsed()
    );
    harness.deck.press(Event::EncoderUp(0));
    assert_eq!(next_mode_change(&events), Some((0, 1)));

    // Its place among the modes, lit over the mode's own colour, then only
    // the colour, which says which mode the knob is in.
    let green = Rgb::new(0, 255, 0);
    assert!(harness.until_ring(0, position(green, 1)));
    assert!(harness.until_ring(0, [green; 4]));

    // Now it turns through pages, and the deck's brightness stays put.
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "two"));
    assert_eq!(harness.status().brightness, 40);
}

#[test]
fn a_hold_that_switches_mode_is_not_also_the_new_or_old_mode_s_press() {
    let harness = Harness::start(
        "no-press",
        &[
            ("galdeck.toml", BRIGHTNESS_AND_PAGES),
            ("profiles/p.toml", PAGES),
        ],
    );
    harness.hold_for_mode(0, 1);
    harness.switch_page("three");

    // A click in the pages mode goes home, on the way up.
    harness.deck.press(Event::EncoderDown(0));
    harness.deck.press(Event::EncoderUp(0));
    assert!(harness.until(|s| s.page == "one"));

    // A hold switches back, and letting go does not go home as well.
    harness.switch_page("three");
    harness.hold_for_mode(0, 0);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(harness.status().page, "three");
    let before = harness.status().brightness;
    harness.turn(0, 1);
    assert!(harness.until(|s| s.brightness == before + 10));
}

#[test]
fn a_knob_keeps_its_mode_through_a_page_without_modes_and_a_reload() {
    let harness = Harness::start(
        "survive",
        &[
            ("galdeck.toml", BRIGHTNESS_AND_PAGES),
            ("profiles/p.toml", PAGES),
        ],
    );
    harness.hold_for_mode(0, 1);

    // Page two shadows the modes with a preset of its own.
    harness.switch_page("two");
    let knob = harness.knob(0);
    assert_eq!(knob.mode, None);
    assert!(knob.modes.is_empty());
    assert_eq!(knob.turn_preset.as_deref(), Some("profiles"));
    assert!(knob.resolved.hold.is_none(), "{:?}", knob.resolved.hold);

    // Back where the modes are, the knob is where it was left.
    harness.switch_page("three");
    let knob = harness.knob(0);
    assert_eq!(knob.mode, Some(1));
    assert_eq!(knob.turn_preset.as_deref(), Some("pages"));

    // A save that leaves the list alone leaves the mode alone.
    harness.reload_with("profiles/p.toml", &PAGES.replace("\"a\"", "\"b\""));
    assert_eq!(harness.knob(0).mode, Some(1));

    // A different list is a different set of modes, from the first.
    harness.reload_with(
        "galdeck.toml",
        &BRIGHTNESS_AND_PAGES.replace(
            "ring = \"#00ff00\" }]",
            "ring = \"#00ff00\" }, \"profiles\"]",
        ),
    );
    let knob = harness.knob(0);
    assert_eq!(knob.modes.len(), 3);
    assert_eq!(knob.mode, Some(0));

    // And changed back, it does not pick up where the old list was.
    harness.reload_with("galdeck.toml", BRIGHTNESS_AND_PAGES);
    assert_eq!(harness.knob(0).mode, Some(0));
}

#[test]
fn a_knob_set_for_every_profile_keeps_its_mode_in_another() {
    let harness = Harness::start(
        "profiles",
        &[
            ("galdeck.toml", BRIGHTNESS_AND_PAGES),
            ("profiles/p.toml", PAGES),
            ("profiles/q.toml", "[[pages]]\nid = \"elsewhere\"\n"),
        ],
    );
    harness.hold_for_mode(0, 1);
    assert!(matches!(
        harness.request(Request::SwitchProfile { name: "q".into() }),
        Response::Ok
    ));
    assert!(harness.until(|s| s.profile == "q"));
    let knob = harness.knob(0);
    assert_eq!(knob.mode, Some(1));
    assert_eq!(knob.turn_preset.as_deref(), Some("pages"));
    // The new profile's page rests the ring at the mode's colour too.
    assert!(harness.until_ring(0, [Rgb::new(0, 255, 0); 4]));
}

#[test]
fn the_layout_lists_a_knob_s_modes_with_where_each_rests() {
    let harness = Harness::start(
        "layout",
        &[
            (
                "galdeck.toml",
                r##"
version = 2
profile = "p"
[[encoders]]
encoder = 1
style = { ring = "#102030" }
modes = ["pages", { preset = "profiles", ring = "#00ff00" }, "deck_brightness"]
"##,
            ),
            ("profiles/p.toml", "[[pages]]\nid = \"one\"\n"),
        ],
    );
    let knob = harness.knob(1);
    assert_eq!(knob.mode, Some(0));
    let titles: Vec<&str> = knob.modes.iter().map(|m| m.title.as_str()).collect();
    assert_eq!(titles, ["Pages", "Profiles", "Deck brightness"]);
    let rings: Vec<Option<&str>> = knob.modes.iter().map(|m| m.ring.as_deref()).collect();
    // The first at the knob's own colour, one at the colour it names, and
    // the third at the first default that looks like neither.
    assert_eq!(rings, [Some("#102030"), Some("#00ff00"), Some("#88c0d0")]);

    // The layer says what it wrote, and only the colours it named.
    let global = knob.layers.iter().find(|l| l.layer == "global").unwrap();
    let written: Vec<(&str, Option<&str>)> = global
        .modes
        .iter()
        .map(|m| (m.preset.as_str(), m.ring.as_deref()))
        .collect();
    assert_eq!(
        written,
        [
            ("pages", None),
            ("profiles", Some("#00ff00")),
            ("deck_brightness", None)
        ]
    );

    // Knob 1 is the right one, and its third mode rests teal.
    harness.hold_for_mode(1, 1);
    harness.hold_for_mode(1, 2);
    assert_eq!(
        harness.knob(1).turn_preset.as_deref(),
        Some("deck_brightness")
    );
    assert!(harness.until_ring(1, [Rgb::new(0x88, 0xc0, 0xd0); 4]));
    // And round again to the first.
    harness.hold_for_mode(1, 0);
}

#[test]
fn a_knob_whose_hold_is_bound_does_not_switch_modes() {
    let harness = Harness::start(
        "bound-hold",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\n[[encoders]]\nencoder = 0\nmodes = [\"pages\", \"profiles\"]\n",
            ),
            (
                "profiles/p.toml",
                "[[pages]]\nid = \"one\"\n[[pages.encoders]]\nencoder = 0\nhold = \"touch '{dir}/held'\"\n",
            ),
        ],
    );
    let knob = harness.knob(0);
    assert_eq!(knob.modes.len(), 2, "the modes are still the knob's turn");
    let hold = knob.resolved.hold.as_ref().unwrap();
    assert_eq!(
        (hold.kind.as_str(), hold.origin.as_deref()),
        ("shell", Some("page"))
    );
    harness.deck.press(Event::EncoderDown(0));
    assert!(harness.wrote("held", Duration::from_secs(3)));
    harness.deck.press(Event::EncoderUp(0));
    assert_eq!(harness.knob(0).mode, Some(0));
}

#[test]
fn the_editor_can_ask_which_outputs_and_apps_there_are() {
    let harness = Harness::start(
        "targets",
        &[
            ("galdeck.toml", "version = 2\nprofile = \"p\"\n"),
            ("profiles/p.toml", "[[pages]]\nid = \"one\"\n"),
        ],
    );
    let mixer = harness.status().capabilities.mixer;
    assert!(
        mixer == "ok" || mixer.starts_with("unavailable: "),
        "{mixer:?}"
    );
    // Only read: whatever this machine has, the answer is a list or why
    // there cannot be one, and never "not built yet".
    for _ in 0..2 {
        match harness.request(Request::AudioTargets) {
            Response::AudioTargets { outputs, .. } => {
                assert_eq!(mixer, "ok");
                assert!(outputs.iter().filter(|o| o.default).count() <= 1);
            }
            Response::Error { message } => {
                assert!(!message.contains("not built"), "{message}");
            }
            other => panic!("not audio targets: {other:?}"),
        }
    }
}

#[test]
fn the_editor_learns_what_needs_the_mixer_and_what_a_target_names() {
    let harness = Harness::start(
        "catalog",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\noutputs = [\"Headphones\", \"Speaker\"]\n",
            ),
            ("profiles/p.toml", "[[pages]]\nid = \"one\"\n"),
        ],
    );
    let Response::Catalog(catalog) = harness.request(Request::Catalog) else {
        panic!("no catalog");
    };
    let preset = |name: &str| catalog.presets.iter().find(|p| p.name == name).unwrap();
    let built_in = |name: &str| catalog.built_ins.iter().find(|b| b.name == name).unwrap();

    // So a card or a picker can say "needs PipeWire" beside exactly these.
    for name in ["outputs", "app_volume"] {
        assert!(preset(name).needs_mixer, "{name}");
    }
    for name in ["volume", "mic", "tracks", "scroll"] {
        assert!(!preset(name).needs_mixer, "{name}");
    }
    for name in ["next_output", "set_output", "app_mute", "next_app"] {
        assert!(built_in(name).needs_mixer, "{name}");
    }
    for name in ["volume_mute", "play_pause", "next_mode", "timer_toggle"] {
        assert!(!built_in(name).needs_mixer, "{name}");
    }

    // What the knob's target field is called, and whether it is there.
    assert_eq!(preset("volume").target_kind.as_deref(), Some("audio_node"));
    assert_eq!(preset("seek").target_kind.as_deref(), Some("player"));
    assert_eq!(preset("app_volume").target_kind.as_deref(), Some("app"));
    assert_eq!(preset("mic").target_kind, None);
    assert_eq!(preset("outputs").target_kind, None);

    let Response::Layout(layout) = harness.request(Request::GetLayout) else {
        panic!("no layout");
    };
    assert_eq!(layout.outputs, ["Headphones", "Speaker"]);
}

/// One patch that appends a table, as the editor sends it.
fn append(path: &str, fields: &[(&str, Value)]) -> Patch {
    Patch::Append {
        path: path.into(),
        fields: fields
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect(),
    }
}

fn remove(path: &str) -> Patch {
    Patch::Remove { path: path.into() }
}

#[test]
fn modes_saved_the_way_the_editor_saves_them_switch_like_written_ones() {
    // A patch cannot write an inline table, so the editor writes a mode with
    // its own step or colour as an array of tables, into a knob entry it may
    // be adding in the same edit. These are its patches, in its order.
    let harness = Harness::start(
        "editor",
        &[
            (
                "galdeck.toml",
                "version = 2\nprofile = \"p\"\nbrightness = 50\n",
            ),
            ("profiles/p.toml", PAGES),
        ],
    );
    let text = |s: &str| Value::String(s.into());
    let response = harness.request(Request::ApplyConfig {
        file: "galdeck.toml".into(),
        patches: vec![
            append("encoders", &[("encoder", Value::Integer(0))]),
            remove("encoders[0].preset"),
            remove("encoders[0].step"),
            remove("encoders[0].target"),
            remove("encoders[0].modes"),
            append(
                "encoders[0].modes",
                &[
                    ("preset", text("deck_brightness")),
                    ("step", Value::Float(10.0)),
                ],
            ),
            append(
                "encoders[0].modes",
                &[("preset", text("pages")), ("ring", text("#00ff00"))],
            ),
        ],
        generation: None,
    });
    assert!(matches!(response, Response::Ok), "got {response:?}");
    let written = std::fs::read_to_string(harness.dir.join("galdeck.toml")).unwrap();
    assert!(written.contains("[[encoders.modes]]"), "{written}");

    let knob = harness.knob(0);
    let presets: Vec<_> = knob.modes.iter().map(|m| m.preset.as_str()).collect();
    assert_eq!(presets, ["deck_brightness", "pages"]);
    assert_eq!(knob.modes[0].step, Some(10.0));
    assert_eq!(knob.modes[1].ring.as_deref(), Some("#00ff00"));
    assert_eq!(knob.mode, Some(0));
    assert_eq!(knob.layers[0].modes.len(), 2);
    let hold = knob.resolved.hold.expect("a hold that switches mode");
    assert_eq!(
        (hold.kind.as_str(), hold.action.as_deref()),
        ("implicit", Some("next_mode"))
    );

    // It switches like modes written by hand.
    harness.turn(0, -1);
    assert!(harness.until(|s| s.brightness == 40));
    harness.hold_for_mode(0, 1);
    harness.turn(0, 1);
    assert!(harness.until(|s| s.page == "two"));

    // Saved back as one preset, the modes go with the rest of what only
    // they used.
    let response = harness.request(Request::ApplyConfig {
        file: "galdeck.toml".into(),
        patches: vec![
            Patch::Set {
                path: "encoders[0].preset".into(),
                value: text("deck_brightness"),
            },
            remove("encoders[0].step"),
            remove("encoders[0].target"),
            remove("encoders[0].modes"),
        ],
        generation: None,
    });
    assert!(matches!(response, Response::Ok), "got {response:?}");
    harness.switch_page("one");
    let knob = harness.knob(0);
    assert!(knob.modes.is_empty() && knob.mode.is_none());
    assert_eq!(knob.turn_preset.as_deref(), Some("deck_brightness"));
    assert!(knob.resolved.hold.is_none());
}
