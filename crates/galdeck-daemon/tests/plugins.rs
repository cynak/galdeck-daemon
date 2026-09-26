//! Plugins are other people's processes. What matters is what they cannot do.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine, EngineParts};
use galdeck_daemon::io::IoThread;
use galdeck_daemon::plugins::PluginHost;
use galdeck_daemon::preview::Preview;
use galdeck_daemon::widgets::WidgetHost;
use galdeck_device::{DeckOp, FakeDeck, FakeDeckHandle};
use galdeck_ipc::{Request, Response};
use galdeck_model::Workspace;

const GLOBAL: &str = "version = 2\nprofile = \"p\"\n";

struct Harness {
    control: ControlSender,
    deck: FakeDeckHandle,
    dir: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start(files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "galdeck-plugins-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, body) in files {
            let path = dir.join(name);
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
                // Deliberately absent: these tests are not about
                // calibration, and must not pick up one from the home
                // directory of whoever is running them.
                calibration_path: Some(
                    std::env::temp_dir().join("galdeck-no-such-calibration.conf"),
                ),
            },
        )
        .expect("engine builds");
        let core = std::thread::spawn(move || engine.run());

        Self {
            control,
            deck: handle,
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
        rx.recv_timeout(Duration::from_secs(5))
            .expect("engine answers")
    }

    /// What key `key` currently shows, according to the layout.
    fn text(&self, key: u8) -> Option<String> {
        let Response::Layout(layout) = self.request(Request::GetLayout) else {
            return None;
        };
        layout
            .keys
            .iter()
            .find(|k| k.key == key)
            .and_then(|k| k.text.clone())
    }

    /// Wait for a key to show something, so tests do not race a child process.
    fn wait_for_text(&self, key: u8, want: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.text(key).as_deref() == Some(want) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn key_writes(&self, key: u8) -> usize {
        self.deck
            .ops()
            .iter()
            .filter(|op| matches!(op, DeckOp::KeyJpeg { key: k, .. } if *k == key))
            .count()
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

/// A plugin that answers hello and then does exactly what it is told to.
const ECHO_PLUGIN: &str = r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    try: m = json.loads(line)
    except ValueError: continue
    t = m.get("type")
    if t == "hello":
        print(json.dumps({"type": "ready", "name": "Echo"}), flush=True)
    elif t == "appear":
        print(json.dumps({"type": "set_text", "key": m["key"], "text": "hello"}), flush=True)
    elif t == "press":
        print(json.dumps({"type": "set_text", "key": m["key"], "text": "pressed"}), flush=True)
    elif t == "shutdown":
        break
"#;

fn manifest(command: &str) -> String {
    format!("name = \"Test\"\ncommand = \"{command}\"\n")
}

fn profile_with_plugin(key: u8, id: &str) -> String {
    format!(
        "[[pages]]\nid = \"main\"\n\n[[pages.keys]]\nkey = {key}\nlabel = \"waiting\"\n\n\
         [pages.keys.plugin]\nid = \"{id}\"\n"
    )
}

#[test]
fn a_plugin_paints_the_key_it_was_given() {
    let harness = Harness::start(&[
        ("galdeck.toml", GLOBAL),
        ("profiles/p.toml", &profile_with_plugin(3, "echo")),
        ("plugins/echo/plugin.toml", &manifest("python3 echo.py")),
        ("plugins/echo/echo.py", ECHO_PLUGIN),
    ]);
    assert!(
        harness.wait_for_text(3, "hello"),
        "the plugin never painted its key; showing {:?}",
        harness.text(3)
    );
}

#[test]
fn a_press_reaches_the_plugin() {
    let harness = Harness::start(&[
        ("galdeck.toml", GLOBAL),
        ("profiles/p.toml", &profile_with_plugin(3, "echo")),
        ("plugins/echo/plugin.toml", &manifest("python3 echo.py")),
        ("plugins/echo/echo.py", ECHO_PLUGIN),
    ]);
    assert!(harness.wait_for_text(3, "hello"));

    harness.deck.press(galdeck::Event::KeyDown(3));
    assert!(
        harness.wait_for_text(3, "pressed"),
        "the press did not reach the plugin"
    );
}

#[test]
fn a_plugin_cannot_paint_a_key_it_was_not_given() {
    // The binding in the config is a grant, not a suggestion. Without this
    // check any plugin could paint over any key on the deck.
    const GREEDY: &str = r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    try: m = json.loads(line)
    except ValueError: continue
    if m.get("type") == "hello":
        print(json.dumps({"type": "ready", "name": "Greedy"}), flush=True)
    elif m.get("type") == "appear":
        for k in range(12):
            print(json.dumps({"type": "set_text", "key": k, "text": "MINE"}), flush=True)
    elif m.get("type") == "shutdown":
        break
"#;
    let profile = "[[pages]]\nid = \"main\"\n\n\
        [[pages.keys]]\nkey = 3\nlabel = \"given\"\n\n[pages.keys.plugin]\nid = \"greedy\"\n\n\
        [[pages.keys]]\nkey = 4\nlabel = \"mine\"\nexec = \"true\"\n";
    let harness = Harness::start(&[
        ("galdeck.toml", GLOBAL),
        ("profiles/p.toml", profile),
        ("plugins/greedy/plugin.toml", &manifest("python3 greedy.py")),
        ("plugins/greedy/greedy.py", GREEDY),
    ]);

    assert!(
        harness.wait_for_text(3, "MINE"),
        "its own key should update"
    );
    // Everything else keeps the label the config gave it.
    assert_eq!(harness.text(4).as_deref(), Some("mine"));
    assert_eq!(harness.text(0), None, "key 0 is not configured at all");
}

#[test]
fn a_plugin_that_never_answers_does_not_stall_the_deck() {
    // The whole reason plugins are separate processes. If any part of this
    // conversation happened on the engine's thread, a plugin that reads
    // nothing and says nothing would take the deck with it.
    let profile = "[[pages]]\nid = \"main\"\n\n\
        [[pages.keys]]\nkey = 3\nlabel = \"waiting\"\n\n[pages.keys.plugin]\nid = \"wedged\"\n\n\
        [[pages.keys]]\nkey = 5\nlabel = \"fine\"\nexec = \"true\"\n";
    let harness = Harness::start(&[
        ("galdeck.toml", GLOBAL),
        ("profiles/p.toml", profile),
        ("plugins/wedged/plugin.toml", &manifest("sleep 300")),
        ("plugins/wedged/nothing.txt", "unused"),
    ]);

    // The rest of the deck painted, and control requests answer promptly.
    assert!(harness.wait_for_text(5, "fine"));
    let started = Instant::now();
    for _ in 0..10 {
        assert!(matches!(harness.request(Request::Ping), Response::Ok));
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "pings took {:?} with a wedged plugin",
        started.elapsed()
    );
}

#[test]
fn a_plugin_that_floods_does_not_grow_the_daemon_without_bound() {
    // A queue somewhere has to be the one that drops. It is better that it be
    // an explicit bound than the allocator.
    const FLOOD: &str = r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    try: m = json.loads(line)
    except ValueError: continue
    if m.get("type") == "hello":
        print(json.dumps({"type": "ready", "name": "Flood"}), flush=True)
    elif m.get("type") == "appear":
        for i in range(20000):
            print(json.dumps({"type": "set_text", "key": m["key"], "text": str(i)}), flush=True)
    elif m.get("type") == "shutdown":
        break
"#;
    let harness = Harness::start(&[
        ("galdeck.toml", GLOBAL),
        ("profiles/p.toml", &profile_with_plugin(3, "flood")),
        ("plugins/flood/plugin.toml", &manifest("python3 flood.py")),
        ("plugins/flood/flood.py", FLOOD),
    ]);

    std::thread::sleep(Duration::from_secs(2));
    // It is allowed to be busy; it is not allowed to take the deck with it.
    let started = Instant::now();
    assert!(matches!(harness.request(Request::Ping), Response::Ok));
    assert!(started.elapsed() < Duration::from_millis(500));
    // And the writes it caused are bounded by the queue, not by 20000.
    assert!(
        harness.key_writes(3) < 5000,
        "a flooding plugin produced {} writes",
        harness.key_writes(3)
    );
}

#[test]
fn a_key_bound_to_a_plugin_that_does_not_exist_is_only_a_warning() {
    // A missing plugin should not stop the rest of the deck from working.
    let harness = Harness::start(&[
        ("galdeck.toml", GLOBAL),
        ("profiles/p.toml", &profile_with_plugin(3, "absent")),
    ]);
    assert_eq!(harness.text(3).as_deref(), Some("waiting"));
    assert!(matches!(harness.request(Request::Ping), Response::Ok));
}
