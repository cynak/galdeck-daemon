//! Two pages of a profile with the same id, driven through a fake deck.
//!
//! The model warns of it (W0103) and loads it all the same, and `next_page`
//! goes to the second page by its place in the list. A key's states and its
//! timer are kept under its page's id, which is the first page's: these
//! check that the second page's keys neither run nor show the first's.
//! Every command here only writes to a file in the test's own directory.

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
    dir: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start(name: &str, files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "galdeck-duplicate-pages-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
        assert!(
            diagnostics.iter().any(|d| d.code == "W0103"),
            "the fixture should have two pages with one id: {diagnostics:#?}"
        );

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
                preview,
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

    /// Wait for key `key` to satisfy `test`, and give it as it was then.
    fn until_key(&self, key: u8, test: impl Fn(&KeyInfo) -> bool) -> KeyInfo {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let info = self.key(key);
            if test(&info) {
                return info;
            }
            assert!(
                Instant::now() < deadline,
                "key {key} never got there: {info:#?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The timer on key `key` of the page showing.
    fn timer(&self, key: u8) -> TimerInfo {
        self.key(key)
            .timer
            .unwrap_or_else(|| panic!("key {key} has no timer"))
    }

    /// A press and release, well short of a hold.
    fn tap(&self, key: u8) {
        self.deck.press(Event::KeyDown(key));
        self.deck.press(Event::KeyUp(key));
    }

    /// On to the page after, by the key on the first page that goes there,
    /// and wait for it to show.
    fn to_second_page(&self, label: &str) {
        self.tap(NEXT);
        self.until_key(0, |info| info.label.as_deref() == Some(label));
    }

    /// Back to the first page: a switch by id goes to the first with it.
    fn to_first_page(&self) {
        assert!(matches!(
            self.request(Request::SwitchPage {
                name: "main".into()
            }),
            Response::Ok
        ));
    }

    /// What the states' commands have written to `ran`, a line each.
    fn ran(&self) -> String {
        std::fs::read_to_string(self.dir.join("ran")).unwrap_or_default()
    }

    /// What the status commands have written to `reads`, a line each.
    fn reads(&self) -> String {
        std::fs::read_to_string(self.dir.join("reads")).unwrap_or_default()
    }

    /// Wait for `ran` to say `lines`.
    fn until_ran(&self, lines: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.ran() != lines {
            assert!(
                Instant::now() < deadline,
                "the commands that ran were {:?}, not {lines:?}",
                self.ran()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
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

/// The icon theme is named, so the engine does not ask the desktop for it.
const GLOBAL: &str = "version = 2\nprofile = \"p\"\nicon_theme = \"Adwaita\"\n";

/// The key on the first page that goes on to the second.
const NEXT: u8 = 1;

/// Two pages called "main": key 0 of each is `first` and `second`, and the
/// first has a key that goes on to the second.
fn two_mains(first: &str, second: &str) -> String {
    format!(
        "[[pages]]\nid = \"main\"\n{first}\
         [[pages.keys]]\nkey = {NEXT}\nlabel = \"Next\"\nexec = {{ action = \"next_page\" }}\n\
         [[pages]]\nid = \"main\"\n{second}"
    )
}

/// Key 0, called `label`, with two states, off and on, each writing its
/// name and `label` to `ran` as it is entered.
fn toggle(label: &str) -> String {
    let state = |name: &str| {
        format!("[[pages.keys.states]]\nname = \"{name}\"\nexec = \"echo {label}-{name} >> '{{dir}}/ran'\"\n")
    };
    format!(
        "[[pages.keys]]\nkey = 0\nlabel = \"{label}\"\n{}{}",
        state("off"),
        state("on")
    )
}

/// [`toggle`], read by a status command that notes each read in `reads`
/// and says `says`.
fn read_toggle(label: &str, says: &str) -> String {
    toggle(label).replacen(
        "\n[[pages.keys.states]]",
        &format!("\nstatus = \"echo {label}-read >> '{{dir}}/reads'; echo {says}\"\n[[pages.keys.states]]"),
        1,
    )
}

/// Key 0, called `label`, with a 25 minute timer.
fn timer(label: &str) -> String {
    format!(
        "[[pages.keys]]\nkey = 0\nlabel = \"{label}\"\nwidget = {{ kind = \"timer\", duration = \"25m\" }}\n"
    )
}

#[test]
fn a_tap_on_the_second_page_with_an_id_runs_nothing_of_the_first_pages_key() {
    let harness = Harness::start(
        "states",
        &[
            ("galdeck.toml", GLOBAL),
            (
                "profiles/p.toml",
                &two_mains(&toggle("Wifi"), &toggle("Bluetooth")),
            ),
        ],
    );
    assert_eq!(harness.key(0).state.as_deref(), Some("off"));

    harness.to_second_page("Bluetooth");
    harness.tap(0);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(harness.ran(), "", "the Wi-Fi key's command ran");
    // The first page keeps the states of every key under "main", so a key
    // here has none to show, and none to change.
    assert_eq!(harness.key(0).state, None);
    match harness.request(Request::SetKeyState {
        key: 0,
        state: "on".into(),
        run: true,
    }) {
        Response::Error { message } => assert!(message.contains("earlier page"), "{message}"),
        other => panic!("not refused: {other:?}"),
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(harness.ran(), "");

    // Nor has anything moved the first page's key.
    harness.to_first_page();
    let wifi = harness.key(0);
    assert_eq!(
        (wifi.label.as_deref(), wifi.state.as_deref()),
        (Some("Wifi"), Some("off"))
    );
    harness.tap(0);
    harness.until_key(0, |info| info.state.as_deref() == Some("on"));
    harness.until_ran("Wifi-on\n");
}

#[test]
fn a_read_on_the_second_page_with_an_id_is_not_of_the_first_pages_key() {
    let harness = Harness::start(
        "reads",
        &[
            ("galdeck.toml", GLOBAL),
            (
                "profiles/p.toml",
                &two_mains(&read_toggle("Wifi", "on"), &read_toggle("Bluetooth", "off")),
            ),
        ],
    );
    harness.until_key(0, |info| {
        info.state.as_deref() == Some("on") && info.state_known
    });
    let reads = harness.reads();

    harness.to_second_page("Bluetooth");
    std::thread::sleep(Duration::from_millis(500));
    // Neither the Wi-Fi key read again for this one, nor what it said
    // shown here.
    assert_eq!(
        harness.reads(),
        reads,
        "the Wi-Fi key was read for this one"
    );
    let bluetooth = harness.key(0);
    assert_eq!(
        (bluetooth.state.as_deref(), bluetooth.state_known),
        (None, false)
    );
    assert!(bluetooth.status_result.is_none(), "{bluetooth:#?}");
}

#[test]
fn a_timer_on_the_second_page_with_an_id_is_not_the_first_pages() {
    let harness = Harness::start(
        "timers",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &two_mains(&timer("Tea"), &timer("Eggs"))),
        ],
    );
    harness.tap(0);
    harness.until_key(0, |info| {
        info.timer.as_ref().is_some_and(|t| t.state == "running")
    });

    harness.to_second_page("Eggs");
    // Its own timer, at its start, rather than the tea counting down.
    let eggs = harness.timer(0);
    assert_eq!(eggs.state, "stopped", "{eggs:?}");
    assert_eq!(eggs.remaining_ms, Some(25 * 60 * 1000));
    // Which a tap does not start -- nor pause the tea's.
    harness.tap(0);
    std::thread::sleep(Duration::from_millis(300));
    let eggs = harness.timer(0);
    assert_eq!(eggs.state, "stopped", "{eggs:?}");

    harness.to_first_page();
    let tea = harness.timer(0);
    assert_eq!(tea.state, "running", "{tea:?}");
}
