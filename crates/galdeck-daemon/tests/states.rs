//! Keys that step through states, driven through a fake deck.
//!
//! The rules for what a tap, a finished command and a read each do to a key
//! are tested over made-up ticks in `states.rs`; these check the engine
//! around them, with real commands on the state runner. Every command here
//! only writes or prints files in the test's own directory: `ran` gets a
//! line each time a state's command runs, so a test can say which ran, in
//! what order, and that nothing ran at all when a key was only shown in a
//! state. Nothing in these tests touches the machine's settings, radios,
//! sound or input devices.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver};
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
use galdeck_ipc::{KeyInfo, Request, Response, Status};
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
            std::env::temp_dir().join(format!("galdeck-states-{name}-{}", std::process::id()));
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

    /// Wait for key `key` to show `state`.
    fn until_state(&self, key: u8, state: &str) -> KeyInfo {
        self.until_key(key, |info| info.state.as_deref() == Some(state))
    }

    /// A press and release, well short of a hold.
    fn tap(&self, key: u8) {
        self.deck.press(Event::KeyDown(key));
        self.deck.press(Event::KeyUp(key));
    }

    fn switch_page(&self, name: &str) {
        assert!(matches!(
            self.request(Request::SwitchPage { name: name.into() }),
            Response::Ok
        ));
    }

    /// Away to page "two" and back to page "one", which reads the states of
    /// the keys on it again.
    fn page_away_and_back(&self) {
        self.switch_page("two");
        self.switch_page("one");
    }

    /// Write the profile over with `body`, as `start` writes a file, and
    /// reload it.
    fn rewrite(&self, body: &str) {
        let body = body.replace("{dir}", &self.dir.display().to_string());
        std::fs::write(self.dir.join("profiles/p.toml"), body).unwrap();
        assert!(matches!(self.request(Request::Reload), Response::Ok));
    }

    fn set_state(&self, key: u8, state: &str, run: bool) -> Response {
        self.request(Request::SetKeyState {
            key,
            state: state.into(),
            run,
        })
    }

    /// What the states' commands have written to `ran`, a line each.
    fn ran(&self) -> String {
        std::fs::read_to_string(self.dir.join("ran")).unwrap_or_default()
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

    /// Key `key`'s picture as the deck was sent it.
    fn picture(&self, key: u8) -> image::RgbImage {
        let jpeg = self.preview.key(key).expect("the key has a picture");
        image::load_from_memory(&jpeg)
            .expect("the key's picture is a JPEG")
            .to_rgb8()
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

/// `keys` on page "one", and a page "two" with nothing on it to switch to.
fn pages(keys: &str) -> String {
    format!(
        "[[pages]]\nid = \"one\"\n{keys}\n[[pages]]\nid = \"two\"\n[[pages.keys]]\nkey = 0\nlabel = \"elsewhere\"\n"
    )
}

/// A state called `name` that writes its name to `ran` as it is entered.
fn state(name: &str) -> String {
    format!("[[pages.keys.states]]\nname = \"{name}\"\nexec = \"echo {name} >> '{{dir}}/ran'\"\n")
}

/// A key with two states, off and on, that cannot read which it is in.
const LAMP: &str = "[[pages.keys]]\nkey = 0\nlabel = \"Lamp\"\n";

fn lamp() -> String {
    format!("{LAMP}{}{}", state("off"), state("on"))
}

/// The next change of state told to `events` about key `key`.
fn told(events: &Receiver<galdeck_ipc::Event>, key: u8) -> (Option<String>, bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(galdeck_ipc::Event::KeyStateChanged {
                key: which,
                state,
                known,
                ..
            }) if which == key => return (state, known),
            Ok(_) => {}
            Err(_) => panic!("no change of state was told for key {key}"),
        }
    }
}

#[test]
fn a_tap_steps_the_key_round_its_states_running_what_each_runs() {
    let keys = format!(
        "[[pages.keys]]\nkey = 0\nlabel = \"Fan\"\n{}{}\nlabel = \"Middle\"\n{}",
        state("low"),
        state("mid"),
        state("high")
    );
    let harness = Harness::start(
        "cycles",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    let key = harness.key(0);
    // With no way to read its state, the key starts in its first, and
    // starting runs nothing.
    assert_eq!(
        (key.state.as_deref(), key.state_known),
        (Some("low"), false)
    );
    let tap = key.tap.expect("a key with states moves on when tapped");
    assert_eq!(
        (tap.kind.as_str(), tap.action.as_deref()),
        ("implicit", Some("next_state"))
    );
    assert_eq!(key.states.len(), 3);
    assert_eq!(key.states[1].label.as_deref(), Some("Middle"));
    assert!(key.states[1].exec.is_some());

    harness.tap(0);
    let mid = harness.until_state(0, "mid");
    assert_eq!(mid.text.as_deref(), Some("Middle"), "{mid:#?}");
    // The key's own label is what an editor writes back, whatever it shows.
    assert_eq!(mid.label.as_deref(), Some("Fan"));
    harness.until_ran("mid\n");
    harness.tap(0);
    harness.until_state(0, "high");
    harness.until_ran("mid\nhigh\n");
    harness.tap(0);
    harness.until_state(0, "low");
    harness.until_ran("mid\nhigh\nlow\n");
}

#[test]
fn a_command_that_fails_takes_the_key_back_and_going_back_runs_nothing() {
    let keys = format!(
        "{LAMP}{}[[pages.keys.states]]\nname = \"on\"\nexec = \"echo 'no lamp here' >&2; exit 3\"\n",
        state("off")
    );
    let harness = Harness::start(
        "fails",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    let events = harness.preview.subscribe();
    harness.tap(0);
    // Shown at once, then taken back when the command fails.
    assert_eq!(told(&events, 0), (Some("on".into()), false));
    assert_eq!(told(&events, 0), (Some("off".into()), false));
    assert_eq!(harness.key(0).state.as_deref(), Some("off"));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(harness.ran(), "", "going back entered nothing");
}

#[test]
fn taps_while_a_command_runs_skip_the_states_between() {
    let keys = format!(
        "{LAMP}{}[[pages.keys.states]]\nname = \"b\"\nexec = \"echo b >> '{{dir}}/ran'; sleep 0.5\"\n{}",
        state("a"),
        state("c")
    );
    let harness = Harness::start(
        "queued",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    harness.tap(0);
    harness.until_ran("b\n");
    // While b's command runs: on to c, and round to a. The key shows each
    // at once, but only the last is entered.
    harness.tap(0);
    harness.until_state(0, "c");
    harness.tap(0);
    harness.until_state(0, "a");
    harness.until_ran("b\na\n");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(harness.ran(), "b\na\n", "c was tapped past, never entered");
    assert_eq!(harness.key(0).state.as_deref(), Some("a"));
}

#[test]
fn a_key_reads_its_state_through_quotes_capitals_and_blank_lines() {
    // A blank line first, then the state in quotes and capitals.
    let quoted = r#"
[[pages.keys]]
key = 0
status = "printf '\n  \"ON\"  \nsecond\n'"
"#;
    // gsettings' way: quotes in the output, and in some of the matches.
    let gsettings = r#"
[[pages.keys]]
key = 1
status = "echo \"'prefer-dark'\""
[[pages.keys.states]]
name = "light"
match = ["'default'", "'prefer-light'"]
[[pages.keys.states]]
name = "dark"
match = ["prefer-dark"]
"#;
    let capitals = "[[pages.keys]]\nkey = 2\nstatus = \"echo Balanced\"\n";
    let keys = [
        quoted.to_string(),
        state("off"),
        state("on"),
        gsettings.to_string(),
        capitals.to_string(),
        state("power-saver"),
        state("balanced"),
        state("performance"),
    ]
    .concat();
    let harness = Harness::start(
        "reads",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    let on = harness.until_key(0, |info| info.state_known);
    assert_eq!(on.state.as_deref(), Some("on"), "{on:#?}");
    let result = on.status_result.expect("what the read found");
    assert!(result.ok && result.error.is_none(), "{result:?}");
    assert_eq!(result.output.as_deref(), Some("\"ON\""));

    let dark = harness.until_key(1, |info| info.state_known);
    assert_eq!(dark.state.as_deref(), Some("dark"));
    let balanced = harness.until_key(2, |info| info.state_known);
    assert_eq!(balanced.state.as_deref(), Some("balanced"));
    assert_eq!(balanced.status.as_deref(), Some("echo Balanced"));
    // A state found by reading it was not entered.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(harness.ran(), "");
}

#[test]
fn a_read_begun_before_the_key_was_set_is_not_taken() {
    let keys = format!(
        "{LAMP}status = \"v=$(cat '{{dir}}/state'); touch '{{dir}}/reading'; sleep 0.8; echo $v\"\n{}{}",
        state("off"),
        state("on")
    );
    let harness = Harness::start(
        "stale",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &pages(&keys)),
            ("state", "off\n"),
        ],
    );
    harness.until_key(0, |info| {
        info.state_known && info.state.as_deref() == Some("off")
    });

    // A read starts, having seen "off"; while it is out, the key is set to
    // "on" by hand, and so is what it stands for.
    let reading = harness.dir.join("reading");
    std::fs::remove_file(&reading).unwrap();
    harness.page_away_and_back();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reading.exists() {
        assert!(Instant::now() < deadline, "the page did not read the key");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(matches!(harness.set_state(0, "on", false), Response::Ok));
    std::fs::write(harness.dir.join("state"), "on\n").unwrap();
    let shown = harness.key(0);
    assert_eq!(
        (shown.state.as_deref(), shown.state_known),
        (Some("on"), false)
    );

    // The read answers "off", and is not believed.
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        assert_eq!(harness.key(0).state.as_deref(), Some("on"));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(harness.ran(), "", "showing a state runs nothing");
}

#[test]
fn showing_a_key_in_a_state_again_never_runs_what_the_state_runs() {
    let keys = format!(
        "{}[[pages.keys]]\nkey = 1\nlabel = \"Read\"\nstatus = \"echo on\"\n{}{}",
        lamp(),
        state("off"),
        state("on")
    );
    let harness = Harness::start(
        "restore",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    harness.until_key(1, |info| {
        info.state_known && info.state.as_deref() == Some("on")
    });
    harness.page_away_and_back();
    harness.until_key(1, |info| info.state_known);
    harness.rewrite(&pages(&keys).replace("\"Read\"", "\"Reads\""));
    harness.until_key(1, |info| {
        info.state_known && info.label.as_deref() == Some("Reads")
    });
    assert!(matches!(harness.set_state(0, "on", false), Response::Ok));
    assert_eq!(harness.key(0).state.as_deref(), Some("on"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(harness.ran(), "");
}

#[test]
fn a_key_keeps_its_state_across_a_page_switch_and_a_reload() {
    let harness = Harness::start(
        "survives",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &pages(&lamp())),
        ],
    );
    harness.tap(0);
    harness.until_ran("on\n");
    harness.switch_page("two");
    assert!(harness.key(0).states.is_empty(), "page two has a plain key");
    harness.switch_page("one");
    assert_eq!(harness.key(0).state.as_deref(), Some("on"));

    // A save that leaves the state be keeps it, by its name however it is
    // spelt now.
    harness.rewrite(&pages(&lamp()).replace("\"on\"", "\"On\""));
    assert_eq!(harness.key(0).state.as_deref(), Some("On"));
    // One that takes it away leaves the key in its first.
    harness.rewrite(&pages(&format!("{LAMP}{}{}", state("off"), state("dim"))));
    assert_eq!(harness.key(0).state.as_deref(), Some("off"));
    // Neither ran anything, and the key still steps on from there.
    harness.tap(0);
    harness.until_state(0, "dim");
    harness.until_ran("on\ndim\n");
}

#[test]
fn switch_to_runs_what_a_state_runs_and_show_only_shows_it() {
    let keys = format!("{}[[pages.keys]]\nkey = 1\nlabel = \"plain\"\n", lamp());
    let harness = Harness::start(
        "set",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    assert!(matches!(harness.set_state(0, "on", true), Response::Ok));
    harness.until_ran("on\n");
    harness.until_state(0, "on");
    // Named in other capitals, it is still the state.
    assert!(matches!(harness.set_state(0, "OFF", false), Response::Ok));
    assert_eq!(harness.key(0).state.as_deref(), Some("off"));

    let refused = |response: Response, says: &str| match response {
        Response::Error { message } => assert!(message.contains(says), "{message}"),
        other => panic!("not refused: {other:?}"),
    };
    refused(harness.set_state(0, "dim", true), "no state called \"dim\"");
    refused(harness.set_state(1, "on", false), "has no states");
    refused(harness.set_state(9, "on", false), "no key 9");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(harness.ran(), "on\n");
}

/// How different two pictures are in the top right corner, where a key says
/// what it knows of its state: the mean difference per channel.
fn corner_difference(a: &image::RgbImage, b: &image::RgbImage) -> f64 {
    let (width, height) = a.dimensions();
    difference(a, b, width * 5 / 8..width, 0..height * 3 / 8)
}

/// How different two pictures are along the middle of the bottom edge,
/// where a key says its command is still running.
fn bottom_difference(a: &image::RgbImage, b: &image::RgbImage) -> f64 {
    let (width, height) = a.dimensions();
    difference(a, b, width * 3 / 10..width * 7 / 10, height * 7 / 8..height)
}

/// The mean difference per channel of two pictures over `xs` by `ys`.
fn difference(
    a: &image::RgbImage,
    b: &image::RgbImage,
    xs: std::ops::Range<u32>,
    ys: std::ops::Range<u32>,
) -> f64 {
    let mut sum = 0.0;
    let mut count = 0.0;
    for y in ys {
        for x in xs.clone() {
            let (p, q) = (a.get_pixel(x, y), b.get_pixel(x, y));
            for channel in 0..3 {
                sum += (f64::from(p[channel]) - f64::from(q[channel])).abs();
            }
            count += 3.0;
        }
    }
    sum / count
}

#[test]
fn a_key_whose_state_cannot_be_read_says_so_until_it_can() {
    let keys = format!(
        "{LAMP}status = \"cat '{{dir}}/state'\"\n{}{}",
        state("off"),
        state("on")
    );
    let harness = Harness::start(
        "unknown",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    let unread = harness.until_key(0, |info| info.status_result.is_some());
    // Not known, so in none of its states, and the editor is told why.
    assert_eq!((unread.state, unread.state_known), (None, false));
    let result = unread.status_result.unwrap();
    assert!(!result.ok);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("No such file")),
        "{result:?}"
    );
    std::thread::sleep(Duration::from_millis(100));
    let with_badge = harness.picture(0);

    std::fs::write(harness.dir.join("state"), "off\n").unwrap();
    harness.page_away_and_back();
    let read = harness.until_key(0, |info| info.state_known);
    assert_eq!(read.state.as_deref(), Some("off"));
    std::thread::sleep(Duration::from_millis(100));
    let without = harness.picture(0);
    let difference = corner_difference(&with_badge, &without);
    assert!(
        difference > 10.0,
        "the corner hardly changed ({difference:.1}) once the state was read"
    );
}

/// Wait for key `key`'s picture to satisfy `test`, saying `what` if it never
/// does.
fn until_picture(harness: &Harness, key: u8, what: &str, test: impl Fn(&image::RgbImage) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !test(&harness.picture(key)) {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_key_whose_command_takes_a_while_says_so_until_it_is_done() {
    // Neither state has a look of its own, so only the bar tells them
    // apart.
    let keys = format!(
        "{LAMP}{}[[pages.keys.states]]\nname = \"on\"\nexec = \"sleep 1; echo on >> '{{dir}}/ran'\"\n",
        state("off")
    );
    let harness = Harness::start(
        "pending",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    let at_rest = harness.picture(0);
    harness.tap(0);
    until_picture(
        &harness,
        0,
        "the key never said its command was running",
        |now| bottom_difference(&at_rest, now) > 10.0,
    );
    harness.until_ran("on\n");
    until_picture(
        &harness,
        0,
        "the key still says so once it is done",
        |now| bottom_difference(&at_rest, now) < 3.0,
    );
    assert_eq!(harness.key(0).state.as_deref(), Some("on"));
}

#[test]
fn a_key_whose_command_failed_says_so_for_a_moment() {
    let keys = format!(
        "{LAMP}{}[[pages.keys.states]]\nname = \"on\"\nexec = \"exit 3\"\n",
        state("off")
    );
    let harness = Harness::start(
        "flash",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    let at_rest = harness.picture(0);
    harness.tap(0);
    until_picture(
        &harness,
        0,
        "the key never said its command failed",
        |now| corner_difference(&at_rest, now) > 10.0,
    );
    // Back in "off", as it was, once the moment is over.
    until_picture(
        &harness,
        0,
        "the key kept saying its command failed",
        |now| corner_difference(&at_rest, now) < 3.0,
    );
    assert_eq!(harness.key(0).state.as_deref(), Some("off"));
    assert_eq!(harness.ran(), "");
}

#[test]
fn every_change_of_state_is_told_as_it_happens() {
    let keys = format!(
        "{}[[pages.keys]]\nkey = 1\nstatus = \"echo on\"\n{}{}",
        lamp(),
        state("off"),
        state("on")
    );
    let harness = Harness::start(
        "told",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    harness.until_key(1, |info| info.state_known);
    let events = harness.preview.subscribe();
    harness.tap(0);
    assert_eq!(told(&events, 0), (Some("on".into()), false));

    // Showing it by hand is a change too, and a read that finds it again
    // says it is known.
    assert!(matches!(harness.set_state(1, "off", false), Response::Ok));
    assert_eq!(told(&events, 1), (Some("off".into()), false));
    harness.page_away_and_back();
    assert_eq!(told(&events, 1), (Some("on".into()), true));
    match events.try_iter().find(|event| {
        matches!(event, galdeck_ipc::Event::KeyStateChanged { profile, page, .. }
            if profile != "p" || page != "one")
    }) {
        None => {}
        Some(event) => panic!("told about somewhere else: {event:?}"),
    }
}

#[test]
fn an_editor_can_draw_a_key_in_each_of_its_states() {
    let keys = lamp()
        .replace(
            "name = \"off\"\n",
            "name = \"off\"\nstyle = { key_bg = \"#000080\" }\n",
        )
        .replace(
            "name = \"on\"\n",
            "name = \"on\"\nstyle = { key_bg = \"#ffff00\" }\n",
        );
    let harness = Harness::start(
        "render",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    let draw = |state: Option<&str>| -> image::RgbImage {
        let response = harness.request(Request::RenderKeyState {
            key: 0,
            state: state.map(String::from),
        });
        let Response::Image { url } = response else {
            panic!("not a picture: {response:?}");
        };
        let data = url
            .strip_prefix("data:image/jpeg;base64,")
            .expect("a JPEG data URL");
        let jpeg = galdeck_daemon::base64::decode(data).expect("base64");
        image::load_from_memory(&jpeg).expect("a JPEG").to_rgb8()
    };
    let corner = |picture: &image::RgbImage| *picture.get_pixel(2, 2);
    let (off, on) = (corner(&draw(Some("off"))), corner(&draw(Some("on"))));
    assert!(off[2] > 100 && off[0] < 40, "{off:?}");
    assert!(on[0] > 200 && on[1] > 200 && on[2] < 60, "{on:?}");
    // With none, it is the key's own look, which is neither.
    let own = corner(&draw(None));
    assert!(own != off && own != on, "{own:?}");
    assert!(matches!(
        harness.request(Request::RenderKeyState {
            key: 0,
            state: Some("dim".into()),
        }),
        Response::Error { .. }
    ));
    // Drawing one runs nothing and moves nothing.
    assert_eq!(harness.key(0).state.as_deref(), Some("off"));
    assert_eq!(harness.ran(), "");
}

#[test]
fn icon_names_and_programs_on_the_path_are_answered() {
    let harness = Harness::start(
        "answers",
        &[
            ("galdeck.toml", GLOBAL),
            ("profiles/p.toml", &pages(&lamp())),
        ],
    );
    let Response::IconNames { names } = harness.request(Request::IconNames) else {
        panic!("not icon names");
    };
    // Whatever this machine has installed: only symbolic names, in order.
    assert!(
        names.iter().all(|name| name.ends_with("-symbolic")),
        "{names:?}"
    );
    assert!(names.windows(2).all(|pair| pair[0] < pair[1]));

    let Response::Which { found } = harness.request(Request::Which {
        names: vec![
            "sh".into(),
            "galdeck-no-such-program".into(),
            "/bin/sh".into(),
        ],
    }) else {
        panic!("not an answer to which");
    };
    assert_eq!(found, ["sh"]);
}

/// How many of `picture`'s pixels are close to `rgb`.
fn pixels_like(picture: &image::RgbImage, rgb: [u8; 3]) -> usize {
    picture
        .pixels()
        .filter(|pixel| (0..3).all(|c| pixel[c].abs_diff(rgb[c]) < 40))
        .count()
}

#[test]
fn a_state_s_icon_is_drawn_in_place_of_the_key_s_own() {
    // Outside the harness's directory, which it clears as it starts.
    let pictures =
        std::env::temp_dir().join(format!("galdeck-state-pictures-{}", std::process::id()));
    std::fs::create_dir_all(&pictures).unwrap();
    for (name, rgb) in [("red", [230, 0, 0]), ("blue", [0, 0, 230])] {
        image::RgbaImage::from_pixel(48, 48, image::Rgba([rgb[0], rgb[1], rgb[2], 255]))
            .save(pictures.join(format!("{name}.png")))
            .unwrap();
    }
    let icon = |name: &str| format!("icon = \"{}/{name}.png\"\n", pictures.display());
    let keys = format!(
        "{LAMP}{}{}{}{}",
        icon("red"),
        state("off"),
        state("on"),
        icon("blue")
    );
    let harness = Harness::start(
        "icons",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    let red = [230, 0, 0];
    let blue = [0, 0, 230];
    let off = harness.picture(0);
    assert!(pixels_like(&off, red) > 1000, "the key's own icon is drawn");
    assert_eq!(pixels_like(&off, blue), 0);

    harness.tap(0);
    harness.until_state(0, "on");
    harness.until_ran("on\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    while pixels_like(&harness.picture(0), blue) < 1000 {
        assert!(Instant::now() < deadline, "the state's icon never showed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pixels_like(&harness.picture(0), red), 0);
    // The key's own is what an editor edits.
    let info = harness.key(0);
    assert!(info.icon.is_some_and(|path| path.ends_with("red.png")));
    assert!(info.states[1]
        .icon
        .as_ref()
        .is_some_and(|path| path.ends_with("blue.png")));
    let _ = std::fs::remove_dir_all(&pictures);
}

#[test]
fn an_icon_name_no_theme_has_is_warned_about_where_it_is_written() {
    let keys = format!(
        "{LAMP}icon = \"galdeck-no-such-icon-symbolic\"\n{}{}icon = \"galdeck-nor-this-one\"\n",
        state("off"),
        state("on")
    );
    let harness = Harness::start(
        "no-icon",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );
    // Once the themes have been found, which is not straight away.
    let deadline = Instant::now() + Duration::from_secs(5);
    let warnings = loop {
        let Response::Layout(layout) = harness.request(Request::GetLayout) else {
            panic!("no layout");
        };
        if !layout.warnings.is_empty() || Instant::now() >= deadline {
            break layout.warnings;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let found: Vec<(&str, &str)> = warnings
        .iter()
        .map(|w| (w.code.as_str(), w.path.as_str()))
        .collect();
    assert_eq!(
        found,
        [
            ("W0197", "pages[0].keys[0].icon"),
            ("W0197", "pages[0].keys[0].states[1].icon"),
        ]
    );
    assert!(warnings[0]
        .message
        .contains("galdeck-no-such-icon-symbolic"));
    // The key is drawn all the same, with its label.
    assert!(harness.preview.key(0).is_some());
}

#[test]
fn a_large_picture_is_shrunk_onto_its_key_and_one_that_cannot_be_drawn_is_warned_about() {
    // Outside the harness's directory, which it clears as it starts.
    let pictures =
        std::env::temp_dir().join(format!("galdeck-state-refused-{}", std::process::id()));
    std::fs::create_dir_all(&pictures).unwrap();
    // An app's 1200 px logo, as the editor's Upload button stores one.
    image::RgbaImage::from_pixel(1200, 1200, image::Rgba([230, 0, 0, 255]))
        .save(pictures.join("logo.png"))
        .unwrap();
    std::fs::write(
        pictures.join("doctype.svg"),
        r#"<!DOCTYPE svg><svg xmlns="http://www.w3.org/2000/svg"/>"#,
    )
    .unwrap();
    let icon = |name: &str| format!("icon = \"{}/{name}\"\n", pictures.display());
    let keys = format!(
        "{LAMP}{}{}{}{}[[pages.keys]]\nkey = 1\nlabel = \"Logo\"\n{}",
        icon("doctype.svg"),
        state("off"),
        state("on"),
        icon("missing.png"),
        icon("logo.png"),
    );
    let harness = Harness::start(
        "refused-icons",
        &[("galdeck.toml", GLOBAL), ("profiles/p.toml", &pages(&keys))],
    );

    let red = [230, 0, 0];
    let deadline = Instant::now() + Duration::from_secs(5);
    while pixels_like(&harness.picture(1), red) < 1000 {
        assert!(Instant::now() < deadline, "the logo was never drawn");
        std::thread::sleep(Duration::from_millis(10));
    }

    let Response::Layout(layout) = harness.request(Request::GetLayout) else {
        panic!("no layout");
    };
    let found: Vec<(&str, &str)> = layout
        .warnings
        .iter()
        .map(|w| (w.code.as_str(), w.path.as_str()))
        .collect();
    assert_eq!(
        found,
        [
            ("W0198", "pages[0].keys[0].icon"),
            ("W0198", "pages[0].keys[0].states[1].icon"),
        ]
    );
    // Saying why, for the editor to show under the field.
    assert!(
        layout.warnings[0].message.contains("DOCTYPE"),
        "{:?}",
        layout.warnings[0]
    );
    assert!(
        layout.warnings[1].message.contains("missing.png"),
        "{:?}",
        layout.warnings[1]
    );
    let _ = std::fs::remove_dir_all(&pictures);
}
