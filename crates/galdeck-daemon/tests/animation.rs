//! An animation has to actually move, and it has to not cost anything to.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::Duration;

use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine};
use galdeck_daemon::io::IoThread;
use galdeck_device::{DeckOp, FakeDeck, FakeDeckHandle};
use galdeck_model::{v1, Workspace};

const ANIMATED: &str = r##"
[[pages]]
name = "main"

[[pages.keys]]
key = 0
label = "Steady"
exec = "true"

[[pages.keys]]
key = 1
label = "Moving"
exec = "true"
"##;

struct Harness {
    _control: ControlSender,
    deck: FakeDeckHandle,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// Boot with a config directory written from `files`.
    fn start(files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "galdeck-anim-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, body) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }

        let (workspace, diagnostics) = Workspace::load(&dir);
        let workspace = workspace.unwrap_or_else(|| panic!("config should load: {diagnostics:#?}"));
        // A file that fails to parse is only a diagnostic, so without this a
        // typo in a fixture makes the test pass for the wrong reason -- which
        // is exactly what it did the first time.
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == galdeck_model::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "fixture config is broken: {errors:#?}");

        let shutdown = Arc::new(AtomicBool::new(false));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let deadline = Arc::new(DeadlineCell::new());
        let (waker, wake_rx) = wake_channel();
        let (paint_tx, paint_rx) = sync_channel(256);
        let (device_tx, device_rx) = sync_channel(256);
        let (control_tx, control_rx) = channel();
        let control = ControlSender::new(control_tx, waker.clone());

        let (deck, handle) = FakeDeck::new();
        let io = IoThread::with_deck(
            Box::new(deck),
            paint_rx,
            device_tx,
            Arc::clone(&deadline),
            Arc::clone(&clock),
            waker,
            Arc::clone(&shutdown),
        );
        let io_thread = std::thread::spawn(move || io.run());

        let mut engine = Engine::new(
            dir,
            workspace,
            control_rx,
            device_rx,
            paint_tx,
            deadline,
            clock,
            wake_rx,
            Arc::clone(&shutdown),
            galdeck_daemon::preview::Preview::new(),
        )
        .expect("engine builds");
        let core = std::thread::spawn(move || engine.run());

        Self {
            _control: control,
            deck: handle,
            shutdown,
            threads: vec![io_thread, core],
        }
    }

    fn key_writes(&self, key: u8) -> usize {
        self.deck
            .ops()
            .iter()
            .filter(|op| matches!(op, DeckOp::KeyJpeg { key: k, .. } if *k == key))
            .count()
    }

    fn ring_writes(&self) -> usize {
        self.deck
            .ops()
            .iter()
            .filter(|op| matches!(op, DeckOp::RingSegment { .. }))
            .count()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn with_animation(extra: &str) -> String {
    format!("version = 2\nprofile = \"p\"\n@@{extra}").replace("@@", "")
}

#[test]
fn an_animated_key_keeps_being_repainted_and_a_still_one_does_not() {
    let profile = r##"
[[pages]]
id = "main"

[[pages.keys]]
key = 0
label = "Steady"
exec = "true"

[[pages.keys]]
key = 1
label = "Moving"
exec = "true"

[pages.keys.animation]
kind = "pulse"
period_ms = 200
frames = 4
"##;
    let harness = Harness::start(&[
        ("galdeck.toml", &with_animation("")),
        ("profiles/p.toml", profile),
    ]);

    // Let the first paint settle, then watch only what follows.
    std::thread::sleep(Duration::from_millis(300));
    harness.deck.clear_ops();
    std::thread::sleep(Duration::from_millis(600));

    let moving = harness.key_writes(1);
    let steady = harness.key_writes(0);
    assert!(
        moving >= 6,
        "a 200 ms cycle over 600 ms should be about twelve frames, got {moving}"
    );
    assert_eq!(steady, 0, "the key with no animation must not be touched");
}

#[test]
fn animation_frames_are_encoded_once_and_then_reused() {
    // The whole design: playing a frame is a channel send, not a JPEG encode.
    // Over many cycles the same handful of byte buffers come back round.
    let profile = r##"
[[pages]]
id = "main"

[[pages.keys]]
key = 0
label = "Moving"
exec = "true"

[pages.keys.animation]
kind = "pulse"
period_ms = 200
frames = 4
"##;
    let harness = Harness::start(&[
        ("galdeck.toml", &with_animation("")),
        ("profiles/p.toml", profile),
    ]);
    std::thread::sleep(Duration::from_millis(900));

    let frames: Vec<Vec<u8>> = harness
        .deck
        .ops()
        .iter()
        .filter_map(|op| match op {
            DeckOp::KeyJpeg { key: 0, jpeg } => Some(jpeg.to_vec()),
            _ => None,
        })
        .collect();
    assert!(
        frames.len() > 6,
        "expected several frames, got {}",
        frames.len()
    );

    let mut distinct = frames.clone();
    distinct.sort();
    distinct.dedup();
    assert!(
        distinct.len() <= 4,
        "a four-frame animation produced {} distinct images",
        distinct.len()
    );
}

#[test]
fn a_ring_animation_runs_without_encoding_anything() {
    let profile = r##"
[[pages]]
id = "main"

[[pages.keys]]
key = 0
label = "Still"
exec = "true"

[[pages.encoders]]
encoder = 0

[pages.encoders.style]
ring = "#204060"

[pages.encoders.animation]
kind = "comet"
period_ms = 240
"##;
    let harness = Harness::start(&[
        ("galdeck.toml", &with_animation("")),
        ("profiles/p.toml", profile),
    ]);
    std::thread::sleep(Duration::from_millis(300));
    harness.deck.clear_ops();
    std::thread::sleep(Duration::from_millis(600));

    assert!(harness.ring_writes() > 4, "the ring should be moving");
    // A ring frame is four colours; no image is produced for it at all.
    let images = harness
        .deck
        .ops()
        .iter()
        .filter(|op| matches!(op, DeckOp::KeyJpeg { .. } | DeckOp::LcdRegion { .. }))
        .count();
    assert_eq!(images, 0, "a ring animation must not encode images");
}

#[test]
fn a_page_with_no_animation_still_goes_quiet() {
    // The regression this guards: scheduling a frame timer for every key,
    // animated or not, would turn an idle daemon into a busy one.
    let harness = Harness::start(&[
        ("galdeck.toml", &with_animation("")),
        (
            "profiles/p.toml",
            "[[pages]]\nid = \"main\"\n\n[[pages.keys]]\nkey = 0\nlabel = \"Still\"\nexec = \"true\"\n",
        ),
    ]);
    std::thread::sleep(Duration::from_millis(300));
    harness.deck.clear_ops();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        harness.deck.ops().is_empty(),
        "an idle page wrote {:?}",
        harness.deck.ops()
    );
}

#[test]
fn the_v1_migration_path_still_works_alongside_animations() {
    let config = v1::Config::parse(ANIMATED).expect("v1 parses");
    let workspace = Workspace::from_v1(&config);
    assert!(workspace
        .profile("default")
        .unwrap()
        .pages
        .iter()
        .all(|page| page.keys.iter().all(|key| key.animation.is_none())));
}
