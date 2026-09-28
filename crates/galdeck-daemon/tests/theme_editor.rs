//! The theme editor's side of the protocol: what a theme sets, what it
//! inherits and from where, starting a new one, and drawing one with edits
//! that are not saved.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::Duration;

use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine};
use galdeck_daemon::io::IoThread;
use galdeck_device::FakeDeck;
use galdeck_ipc::{Patch, Request, Response, Severity, ThemeInfo, ThemePreview, Value};
use galdeck_model::Workspace;

struct Harness {
    control: ControlSender,
    dir: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// The shipped example, copied so a test that saves does not edit the
    /// repository: `nord`, and `nord-bright` extending it.
    fn start() -> Self {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/v2");
        let dir = std::env::temp_dir().join(format!(
            "galdeck-theme-editor-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&source, &dir);

        let (workspace, diagnostics) = Workspace::load(&dir);
        let workspace = workspace.expect("the example must load");
        assert!(
            !diagnostics.iter().any(|d| d.severity == Severity::Error),
            "{diagnostics:#?}"
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let deadline = Arc::new(DeadlineCell::new());
        let (waker, wake_rx) = wake_channel();
        let (paint_tx, paint_rx) = sync_channel(64);
        let (device_tx, device_rx) = sync_channel(256);
        let (control_tx, control_rx) = channel();
        let control = ControlSender::new(control_tx, waker.clone());

        let (widget_host, widget_rx) = galdeck_daemon::widgets::WidgetHost::new(waker.clone());
        let (plugin_host, plugin_rx) =
            galdeck_daemon::plugins::PluginHost::discover(&dir, waker.clone());

        let (deck, _handle) = FakeDeck::new();
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
            galdeck_daemon::engine::EngineParts {
                control_rx,
                device_rx,
                paint_tx,
                widget_rx,
                wake: wake_rx,
                deadline,
                clock,
                shutdown: Arc::clone(&shutdown),
                preview: galdeck_daemon::preview::Preview::new(),
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

        Self {
            control,
            dir,
            shutdown,
            threads: vec![io_thread, core],
        }
    }

    fn request(&self, request: Request) -> Response {
        let (reply, rx) = channel();
        self.control
            .send(ControlMsg { request, reply })
            .expect("engine listening");
        rx.recv_timeout(Duration::from_secs(10))
            .expect("engine answers")
    }

    fn themes(&self) -> Vec<ThemeInfo> {
        match self.request(Request::GetThemes) {
            Response::Themes { themes } => themes,
            other => panic!("expected themes, got {other:?}"),
        }
    }

    fn theme(&self, id: &str) -> ThemeInfo {
        self.themes()
            .into_iter()
            .find(|theme| theme.id == id)
            .unwrap_or_else(|| panic!("no theme {id}"))
    }

    fn preview(&self, theme: &str, patches: Vec<Patch>) -> ThemePreview {
        match self.request(Request::PreviewTheme {
            theme: theme.into(),
            patches,
        }) {
            Response::ThemePreview(preview) => *preview,
            other => panic!("expected a preview, got {other:?}"),
        }
    }

    fn create(&self, id: &str, extends: Option<&str>, copy: Option<&str>) -> Response {
        self.request(Request::CreateTheme {
            id: id.into(),
            name: Some(format!("The {id} theme")),
            extends: extends.map(Into::into),
            copy: copy.map(Into::into),
        })
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.dir.join(file)).unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

fn set(path: &str, value: &str) -> Patch {
    Patch::Set {
        path: path.into(),
        value: Value::String(value.into()),
    }
}

/// The colour of a preview picture near its top-left corner, where there is
/// nothing but the key's background.
fn corner(url: &str) -> [u8; 3] {
    let data = url
        .strip_prefix("data:image/jpeg;base64,")
        .expect("a JPEG data: URL");
    let bytes = galdeck_daemon::base64::decode(data).expect("base64");
    let image = image::load_from_memory(&bytes)
        .expect("a picture")
        .to_rgb8();
    image.get_pixel(3, 3).0
}

fn near(actual: [u8; 3], expected: [u8; 3]) -> bool {
    // JPEG moves a flat colour by a few steps.
    actual
        .iter()
        .zip(expected)
        .all(|(a, e)| (i16::from(*a) - i16::from(e)).abs() <= 12)
}

#[test]
fn a_theme_says_what_it_sets_and_what_it_inherits_from_where() {
    let harness = Harness::start();
    let bright = harness.theme("nord-bright");
    assert_eq!(bright.file, "themes/nord-bright.toml");
    assert_eq!(bright.extends.as_deref(), Some("nord"));
    assert_eq!(bright.ancestors, ["nord"]);
    assert_eq!(bright.profiles, ["play"]);

    let colour = |name: &str| {
        bright
            .palette
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("no palette entry {name}"))
    };
    // Its own, and one it only inherits.
    assert_eq!(colour("storm").origin, "nord-bright");
    assert_eq!(colour("storm").hex.as_deref(), Some("#4c566a"));
    assert_eq!(colour("frost").origin, "nord");
    assert_eq!(colour("frost").value, "#88c0d0");
    // `accent` is its own, even though nord has one too: listed once.
    assert_eq!(
        bright.palette.iter().filter(|e| e.name == "accent").count(),
        1
    );

    let field = |name: &str| {
        bright
            .style
            .iter()
            .find(|field| field.field == name)
            .unwrap_or_else(|| panic!("no style field {name}"))
    };
    assert_eq!(field("key_label_size").origin, "nord-bright");
    assert_eq!(field("key_label_size").value.as_deref(), Some("30"));
    assert_eq!(field("key_label_size").kind, "size");
    // Emptied, it would take nord's.
    assert_eq!(field("key_label_size").inherited, "26");
    assert_eq!(field("key_label_size").inherited_origin, "nord");
    // nord says `key_bg = "@storm"`, and nord-bright redefined storm: the
    // field is nord's, and its colour is nord-bright's.
    assert_eq!(field("key_bg").origin, "nord");
    assert_eq!(field("key_bg").value, None);
    assert_eq!(field("key_bg").resolved, "#4c566a");
    assert_eq!(field("key_label_strip").kind, "pixels");

    let nord = harness.theme("nord");
    assert_eq!(nord.extended_by, ["nord-bright"]);
    assert!(nord.ancestors.is_empty());
    let size = nord
        .style
        .iter()
        .find(|f| f.field == "key_label_size")
        .unwrap();
    assert_eq!(
        (size.inherited.as_str(), size.inherited_origin.as_str()),
        ("26", "builtin")
    );
    assert_eq!(
        nord.style
            .iter()
            .find(|f| f.field == "key_bg")
            .unwrap()
            .value
            .as_deref(),
        Some("@storm")
    );
}

#[test]
fn a_new_theme_that_extends_another_inherits_everything_and_loads() {
    let harness = Harness::start();
    assert!(matches!(
        harness.create("mine", Some("nord"), None),
        Response::Ok
    ));
    let text = harness.read("themes/mine.toml");
    assert!(text.contains("name = \"The mine theme\""), "{text}");
    assert!(text.contains("extends = \"nord\""), "{text}");

    let mine = harness.theme("mine");
    assert_eq!(mine.ancestors, ["nord"]);
    assert!(mine.style.iter().all(|field| field.origin == "nord"));
    assert!(mine.style.iter().all(|field| field.value.is_none()));
    assert!(harness
        .theme("nord")
        .extended_by
        .contains(&"mine".to_string()));
}

#[test]
fn an_empty_theme_leaves_everything_to_the_built_in_look() {
    let harness = Harness::start();
    assert!(matches!(harness.create("blank", None, None), Response::Ok));
    let blank = harness.theme("blank");
    assert!(blank.palette.is_empty());
    assert!(blank.style.iter().all(|field| field.origin == "builtin"));
    let key_bg = blank.style.iter().find(|f| f.field == "key_bg").unwrap();
    assert_eq!(key_bg.resolved, "#181a20");
}

#[test]
fn a_copy_keeps_the_comments_and_takes_its_new_name() {
    let harness = Harness::start();
    assert!(matches!(
        harness.create("copied", None, Some("nord")),
        Response::Ok
    ));
    let text = harness.read("themes/copied.toml");
    assert!(
        text.contains("# A theme is a palette plus a set of style defaults."),
        "{text}"
    );
    assert!(text.contains("name = \"The copied theme\""), "{text}");
    assert!(!text.contains("name = \"Nord\""), "{text}");
    let copied = harness.theme("copied");
    assert!(copied.ancestors.is_empty());
    assert!(copied.palette.iter().all(|entry| entry.origin == "copied"));
}

#[test]
fn a_theme_is_never_started_over_one_that_is_there_or_outside_themes() {
    let harness = Harness::start();
    let before = harness.read("themes/nord.toml");
    assert!(matches!(
        harness.create("nord", Some("nord-bright"), None),
        Response::Error { .. }
    ));
    assert_eq!(harness.read("themes/nord.toml"), before);

    for id in ["../escape", "Nord", "", "a.b"] {
        assert!(
            matches!(harness.create(id, None, None), Response::Error { .. }),
            "{id:?} was accepted"
        );
    }
    assert!(!harness.dir.join("escape.toml").exists());
    assert!(matches!(
        harness.create("both", Some("nord"), Some("nord")),
        Response::Error { .. }
    ));
    assert!(matches!(
        harness.create("orphan", Some("no-such-theme"), None),
        Response::Error { .. }
    ));
    assert!(!harness.dir.join("themes/orphan.toml").exists());
}

#[test]
fn a_preview_draws_edits_that_are_not_saved_and_saves_nothing() {
    let harness = Harness::start();
    let before = harness.read("themes/nord.toml");
    // Every key's background is `@storm`; moving storm moves them all.
    let preview = harness.preview("nord", vec![set("palette.storm", "#ff0000")]);
    assert!(preview.diagnostics.is_empty(), "{:#?}", preview.diagnostics);
    assert_eq!(preview.keys.len(), 5);
    assert!(near(corner(&preview.keys[0]), [255, 0, 0]));
    assert!(preview.lcd.starts_with("data:image/jpeg;base64,"));
    assert_eq!(preview.ring, "#88c0d0");
    let storm = preview.palette.iter().find(|e| e.name == "storm").unwrap();
    assert_eq!(storm.hex.as_deref(), Some("#ff0000"));
    assert_eq!(harness.read("themes/nord.toml"), before);

    // And without the edit, the saved colour.
    let saved = harness.preview("nord", Vec::new());
    assert!(near(corner(&saved.keys[0]), [0x3b, 0x42, 0x52]));
}

#[test]
fn a_preview_of_a_broken_edit_is_drawn_anyway_and_says_what_is_wrong() {
    let harness = Harness::start();
    let preview = harness.preview("nord", vec![set("style.key_bg", "@no-such-colour")]);
    assert!(
        preview.diagnostics.iter().any(|d| d.code == "E0117"),
        "{:#?}",
        preview.diagnostics
    );
    assert_eq!(preview.keys.len(), 5);
}

#[test]
fn lighting_is_described_as_written_and_previewed_resolved() {
    let harness = Harness::start();
    let preview = harness.preview(
        "nord-bright",
        vec![
            set("lighting.effect", "wave"),
            Patch::Set {
                path: "lighting.colors".into(),
                value: Value::Array(vec![
                    Value::String("@frost".into()),
                    Value::String("#ff0000".into()),
                ]),
            },
            set("lighting.keys.w a s d", "@accent"),
        ],
    );
    assert!(preview.diagnostics.is_empty(), "{:#?}", preview.diagnostics);
    let lighting = preview.lighting.expect("lighting");
    assert_eq!(lighting.effect.as_deref(), Some("wave"));
    assert_eq!(
        lighting.colors_hex,
        [Some("#88c0d0".to_string()), Some("#ff0000".to_string())]
    );
    assert_eq!(lighting.keys.len(), 1);
    assert_eq!(lighting.keys[0].keys, "w a s d");
    assert_eq!(lighting.keys[0].hex.as_deref(), Some("#a3be8c"));

    // A theme without any says so, rather than describing an empty one.
    assert!(harness.theme("nord").lighting.is_none());
    assert!(harness.theme("nord").inherited_lighting.is_none());
}

#[test]
fn widget_looks_are_described_and_drawn_in_the_preview() {
    let harness = Harness::start();
    let plain = harness.preview("nord", Vec::new());
    let looked = harness.preview(
        "nord",
        vec![
            set("widgets.clock.view", "nixie"),
            set("widgets.bar", "segmented"),
            set("widgets.color", "@aurora-green"),
        ],
    );
    assert!(looked.diagnostics.is_empty(), "{:#?}", looked.diagnostics);
    // The gauge, the bar and the clock all change; the plain key does not.
    assert_eq!(plain.keys[0], looked.keys[0]);
    for (index, what) in [(1, "gauge"), (3, "bar"), (4, "clock")] {
        assert_ne!(
            plain.keys[index], looked.keys[index],
            "the {what} did not change"
        );
    }

    // Saved, it is described as written, and what extends it inherits it.
    let saved = harness.request(Request::ApplyConfig {
        file: "themes/nord.toml".into(),
        patches: vec![
            set("widgets.clock.view", "nixie"),
            set("widgets.color", "@aurora-green"),
        ],
        generation: None,
    });
    assert!(matches!(saved, Response::Ok), "{saved:?}");
    let nord = harness.theme("nord");
    let widgets = nord.widgets.expect("nord's [widgets]");
    assert_eq!(widgets.all.color.as_deref(), Some("@aurora-green"));
    assert_eq!(widgets.all.color_hex.as_deref(), Some("#a3be8c"));
    assert_eq!(widgets.kinds.len(), 1);
    assert_eq!(widgets.kinds[0].kind, "clock");
    assert_eq!(widgets.kinds[0].look.view.as_deref(), Some("nixie"));
    let bright = harness.theme("nord-bright");
    assert!(bright.widgets.is_none());
    let inherited = bright.inherited_widgets.expect("nord's, through extends");
    assert_eq!(inherited.kinds[0].look.view.as_deref(), Some("nixie"));
}

#[test]
fn motion_is_described_and_a_press_is_drawn_in_the_preview() {
    let harness = Harness::start();
    let still = harness.preview("nord", Vec::new());
    assert_eq!(still.keys.len(), 5, "no press, no pressed key");
    assert!(still.motion.is_none());

    let pressing = harness.preview(
        "nord",
        vec![
            set("motion.press.kind", "flash"),
            set("motion.press.color", "@aurora-red"),
            set("motion.rings.kind", "breathe"),
        ],
    );
    assert!(
        pressing.diagnostics.is_empty(),
        "{:#?}",
        pressing.diagnostics
    );
    assert_eq!(pressing.keys.len(), 6, "the last is a key mid-press");
    // Tinted towards the flash colour, away from the key's own storm grey.
    assert!(
        !near(corner(&pressing.keys[5]), [0x3b, 0x42, 0x52]),
        "the pressed key is tinted"
    );
    let motion = pressing.motion.expect("the theme's motion");
    assert_eq!(
        motion.rings.as_ref().map(|r| r.kind.as_str()),
        Some("breathe")
    );

    let saved = harness.request(Request::ApplyConfig {
        file: "themes/nord.toml".into(),
        patches: vec![
            set("motion.press.kind", "dim"),
            set("motion.alarm.kind", "pulse"),
        ],
        generation: None,
    });
    assert!(matches!(saved, Response::Ok), "{saved:?}");
    let nord = harness.theme("nord").motion.expect("nord's [motion]");
    assert_eq!(nord.press.map(|p| p.kind), Some("dim".to_string()));
    assert_eq!(nord.alarm.map(|a| a.kind), Some("pulse".to_string()));
    let inherited = harness
        .theme("nord-bright")
        .inherited_motion
        .expect("nord's, through extends");
    assert_eq!(inherited.alarm.map(|a| a.period_ms), Some(2000));
}
