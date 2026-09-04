//! What the thread split is actually for.
//!
//! Three claims, each of which was false before it: control requests do not
//! wait on the device, a page switch writes only what changed, and an idle
//! daemon writes nothing at all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine};
use galdeck_daemon::io::IoThread;
use galdeck_device::{DeckOp, FakeDeck, FakeDeckHandle};
use galdeck_ipc::{Request, Response};
use galdeck_model::{v1, Workspace};

const CONFIG: &str = r##"
brightness = 60
lcd_text = "test"

[[pages]]
name = "main"

[[pages.keys]]
key = 0
label = "One"
color = "#203040"
exec = "true"

[[pages.keys]]
key = 1
label = "Two"
color = "#302040"
page = "other"

[[pages.encoders]]
encoder = 0
ring = "#00c896"

[[pages]]
name = "other"
lcd_text = "the other page"

[[pages.keys]]
key = 0
label = "One"
color = "#203040"
exec = "true"

[[pages.keys]]
key = 1
label = "Back"
color = "#402020"
page = "main"

[[pages.encoders]]
encoder = 0
ring = "#00c896"
"##;

struct Harness {
    control: ControlSender,
    deck: FakeDeckHandle,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start() -> Self {
        let config = v1::Config::parse(CONFIG).expect("test config should parse");
        let workspace = Workspace::from_v1(&config);
        let shutdown = Arc::new(AtomicBool::new(false));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let deadline = Arc::new(DeadlineCell::new());
        let (waker, wake_rx) = wake_channel();
        let (paint_tx, paint_rx) = sync_channel(64);
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
            "test-config".into(),
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
        .expect("engine should build");
        let core_thread = std::thread::spawn(move || engine.run());

        Self {
            control,
            deck: handle,
            shutdown,
            threads: vec![io_thread, core_thread],
        }
    }

    fn request(&self, request: Request) -> Response {
        let (reply, rx) = channel();
        self.control
            .send(ControlMsg { request, reply })
            .expect("engine should be listening");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("engine should answer")
    }

    /// Wait until the deck has been painted and then stops changing, so
    /// assertions are not racing paint that has not started yet.
    fn settle(&self) {
        let start = Instant::now();
        while self.deck.surface().generation == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "nothing was ever painted"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Require sustained quiet, not one quiet sample: a budgeted flush
        // alternates writing with polling, so there are lulls in the middle of
        // a page paint that a single sample would mistake for the end of it.
        let mut last = self.deck.surface().generation;
        let mut quiet = 0;
        for _ in 0..400 {
            std::thread::sleep(Duration::from_millis(10));
            let now = self.deck.surface().generation;
            quiet = if now == last { quiet + 1 } else { 0 };
            last = now;
            if quiet >= 5 {
                return;
            }
        }
        panic!("the deck never stopped changing");
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

#[test]
fn a_control_request_does_not_wait_on_the_device() {
    // This used to be bounded by the poll timeout, because control requests
    // were drained once per device poll. Two hundred milliseconds for a ping.
    let harness = Harness::start();
    harness.settle();

    let mut worst = Duration::ZERO;
    for _ in 0..20 {
        let started = Instant::now();
        assert!(matches!(harness.request(Request::Ping), Response::Ok));
        worst = worst.max(started.elapsed());
    }
    assert!(
        worst < Duration::from_millis(20),
        "worst ping was {worst:?}; the core thread is blocking on something"
    );
}

#[test]
fn a_page_switch_writes_only_what_changed() {
    // The two pages differ in exactly one key. Before diffing, this cost
    // twelve key images, eight ring segments and a full 720x384 LCD frame.
    let harness = Harness::start();
    harness.settle();
    harness.deck.clear_ops();

    assert!(matches!(
        harness.request(Request::SwitchPage {
            name: "other".into()
        }),
        Response::Ok
    ));
    harness.settle();

    let ops = harness.deck.ops();
    let keys = ops
        .iter()
        .filter(|op| matches!(op, DeckOp::KeyJpeg { .. } | DeckOp::KeyClear { .. }))
        .count();
    let rings = ops
        .iter()
        .filter(|op| matches!(op, DeckOp::RingSegment { .. }))
        .count();
    let lcd = ops
        .iter()
        .filter(|op| matches!(op, DeckOp::LcdRegion { .. }))
        .count();

    assert_eq!(keys, 1, "only key 1 differs between the pages: {ops:#?}");
    assert_eq!(rings, 0, "both pages rest the ring at the same colour");
    assert_eq!(lcd, 1, "the pages carry different lcd text");
}

#[test]
fn an_idle_daemon_writes_nothing() {
    let harness = Harness::start();
    harness.settle();
    harness.deck.clear_ops();

    std::thread::sleep(Duration::from_millis(600));
    assert!(
        harness.deck.ops().is_empty(),
        "an idle daemon wrote {:?}",
        harness.deck.ops()
    );
}

#[test]
fn the_first_paint_covers_every_surface() {
    // Nothing is known about a freshly opened device -- and `take_mode_reentry`
    // deliberately does not fire after open -- so the opening paint must be
    // unconditional.
    let harness = Harness::start();
    harness.settle();

    let ops = harness.deck.ops();
    let keys = ops
        .iter()
        .filter(|op| matches!(op, DeckOp::KeyJpeg { .. } | DeckOp::KeyClear { .. }))
        .count();
    assert_eq!(keys, galdeck::Buttons::COUNT as usize);
    assert!(ops.iter().any(|op| matches!(op, DeckOp::Brightness(60))));
    assert!(ops.iter().any(|op| matches!(op, DeckOp::LcdRegion { .. })));
}

#[test]
fn the_deck_is_never_handed_a_malformed_image() {
    // The fake decodes what it is given, so this catches a key image that is
    // not 160x160 or an LCD patch that does not match its rectangle.
    let harness = Harness::start();
    harness.settle();
    harness.request(Request::SwitchPage {
        name: "other".into(),
    });
    harness.settle();

    assert!(
        harness.deck.violations().is_empty(),
        "{:?}",
        harness.deck.violations()
    );
}
