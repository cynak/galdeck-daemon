//! Handing the device to another process, and taking it back.
//!
//! The wizard that measures panel geometry needs the physical knobs, so it
//! runs outside the daemon -- `galdeck calibrate` -- and the daemon has to get
//! out of its way. The guarantee that makes that safe is narrow and easy to
//! get wrong: `ReleaseDevice` answers `Ok` only once the hidraw handle is
//! closed, because the caller opens the node the instant it reads the reply,
//! and two handles on one node is the leading suspect for the module dropping
//! off the USB bus.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine};
use galdeck_daemon::io::IoThread;
use galdeck_device::{FakeDeck, FakeDeckHandle};
use galdeck_ipc::{Request, Response, Status};
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
"##;

struct Harness {
    control: ControlSender,
    deck: FakeDeckHandle,
    /// Held so tests can subscribe. A queue, not a sample: whether the io
    /// thread reopened the device is a thing that *happened*, and polling
    /// `Status` for it misses a device that was reopened and dropped again
    /// between two reads -- which is exactly what a broken park loop does.
    preview: galdeck_daemon::preview::Preview,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start() -> Self {
        let config = v1::Config::parse(CONFIG).expect("test config should parse");
        let workspace = Workspace::from_v1(&config);
        let shutdown = Arc::new(AtomicBool::new(false));
        // One flag, shared: the engine sets it and the io thread reads it.
        // Two would make every test here pass while the daemon deadlocked.
        let parked = Arc::new(AtomicBool::new(false));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let deadline = Arc::new(DeadlineCell::new());
        let (waker, wake_rx) = wake_channel();
        let (paint_tx, paint_rx) = sync_channel(64);
        let (device_tx, device_rx) = sync_channel(256);
        let (control_tx, control_rx) = channel();
        let control = ControlSender::new(control_tx, waker.clone());

        let preview = galdeck_daemon::preview::Preview::new();
        let (widget_host, widget_rx) = galdeck_daemon::widgets::WidgetHost::new(waker.clone());
        let (plugin_host, plugin_rx) = galdeck_daemon::plugins::PluginHost::discover(
            std::path::Path::new("test-config"),
            waker.clone(),
        );

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
            "test-config".into(),
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
                preview: preview.clone(),
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
        .expect("engine should build");
        let core_thread = std::thread::spawn(move || engine.run());

        Self {
            control,
            deck: handle,
            preview,
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

    fn status(&self) -> Status {
        match self.request(Request::Status) {
            Response::Status(status) => status,
            other => panic!("expected a status, got {other:?}"),
        }
    }

    fn events(&self) -> std::sync::mpsc::Receiver<galdeck_ipc::Event> {
        self.preview.subscribe()
    }

    /// Wait until the opening paint has landed, so a later silence means
    /// something.
    fn settle(&self) {
        let start = Instant::now();
        while self.deck.surface().generation == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "nothing was ever painted"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut last = self.deck.surface().generation;
        let mut quiet = 0;
        for _ in 0..400 {
            std::thread::sleep(Duration::from_millis(10));
            let now = self.deck.surface().generation;
            quiet = if now == last { quiet + 1 } else { 0 };
            last = now;
            // Twenty, not five: the info screen is encoded on the core
            // thread and sent after every key, so it lands well after they
            // do, and a shorter window decides the deck is idle while a
            // frame is still in flight. See the note in two_threads.rs.
            if quiet >= 20 {
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
fn ok_to_a_release_means_the_handle_is_already_closed() {
    // The whole point. If the reply were sent when the request was handled
    // rather than when the io thread confirmed the close, this would still
    // say Ok -- and the device would still be open, which is the bug.
    let harness = Harness::start();
    harness.settle();
    assert!(harness.status().connected, "the fake should be attached");

    assert!(matches!(
        harness.request(Request::ReleaseDevice),
        Response::Ok
    ));

    // Read straight after the Ok, with no settling in between: the engine
    // only answers a release from inside its Disconnected handler, so by the
    // time the reply is readable the device is already recorded as gone.
    let status = harness.status();
    assert!(
        !status.connected,
        "answered while the device was still open"
    );
    assert!(status.released, "status should say why it is disconnected");
}

#[test]
fn a_released_device_is_not_quietly_reopened() {
    // The io thread reconnects on its own, every two seconds, which is
    // exactly the behaviour that has to stop while someone else holds the
    // node. A missing `continue` in the park branch would show up here and
    // essentially nowhere else.
    let harness = Harness::start();
    harness.settle();

    // Watched on the event queue rather than polled through Status. A park
    // loop that falls through to try_connect reopens the device and drops it
    // again one pass later, so it is connected for about a tenth of a
    // reconnect interval -- and two versions of this test that sampled
    // Status, one of them sampling every 20ms, both passed against a
    // deliberately broken loop. The event is the thing that cannot be missed.
    let events = harness.events();
    harness.request(Request::ReleaseDevice);
    std::thread::sleep(Duration::from_millis(2500));

    let reopened = events
        .try_iter()
        .filter(|event| matches!(event, galdeck_ipc::Event::DeviceConnected { .. }))
        .count();
    assert_eq!(
        reopened, 0,
        "the io thread reopened a device it had handed over"
    );
    assert!(harness.status().released);
}

#[test]
fn resuming_takes_the_device_back() {
    let harness = Harness::start();
    harness.settle();
    harness.request(Request::ReleaseDevice);
    assert!(!harness.status().connected);

    assert!(matches!(
        harness.request(Request::ResumeDevice),
        Response::Ok
    ));

    let start = Instant::now();
    loop {
        let status = harness.status();
        if status.connected {
            assert!(!status.released);
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the device never came back"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn releasing_twice_answers_twice() {
    // The second release has no Disconnected coming -- it already happened --
    // so a handler that always deferred would hang the caller forever.
    let harness = Harness::start();
    harness.settle();

    assert!(matches!(
        harness.request(Request::ReleaseDevice),
        Response::Ok
    ));
    assert!(matches!(
        harness.request(Request::ReleaseDevice),
        Response::Ok
    ));
}

#[test]
fn a_stray_resume_is_harmless() {
    let harness = Harness::start();
    harness.settle();

    assert!(matches!(
        harness.request(Request::ResumeDevice),
        Response::Ok
    ));
    assert!(harness.status().connected);
}

#[test]
fn control_requests_are_still_answered_while_parked() {
    // A parked io thread must not take the control mailbox down with it: the
    // UI is still up, and the page that asked for the handover is the one
    // that has to be able to ask for it back.
    let harness = Harness::start();
    harness.settle();
    harness.request(Request::ReleaseDevice);

    let mut worst = Duration::ZERO;
    for _ in 0..20 {
        let started = Instant::now();
        assert!(matches!(harness.request(Request::Ping), Response::Ok));
        worst = worst.max(started.elapsed());
    }
    assert!(
        worst < Duration::from_millis(50),
        "worst ping while parked was {worst:?}"
    );
}

#[test]
fn nothing_is_written_to_a_device_that_was_handed_over() {
    let harness = Harness::start();
    harness.settle();
    harness.request(Request::ReleaseDevice);
    harness.deck.clear_ops();

    // Paint that would otherwise reach the panel. The io thread drains it and
    // throws it away rather than letting the channel back up.
    harness.request(Request::SetBrightness {
        percent: 30,
        device: galdeck_ipc::DeckDevice::All,
    });
    std::thread::sleep(Duration::from_millis(400));

    assert!(
        harness.deck.ops().is_empty(),
        "wrote to a device it had handed over: {:?}",
        harness.deck.ops()
    );
}
