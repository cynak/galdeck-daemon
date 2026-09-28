//! Reading and editing the calibration over the control protocol.
//!
//! Each test owns a layout file in its own directory and hands the engine
//! the path. An earlier version set `$XDG_CONFIG_HOME` instead, which is
//! process-global: two tests racing for one file failed in a way that looked
//! like a bug in the daemon, and the suite quietly read the calibration of
//! whoever happened to be running it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::Duration;

use galdeck::layout::{Band, Layout, Rect, Source};
use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine};
use galdeck_daemon::io::IoThread;
use galdeck_device::{DeckOp, FakeDeck, FakeDeckHandle};
use galdeck_ipc::{CalRect, CalSource, Calibration, Request, Response};
use galdeck_model::{v1, Workspace};

const CONFIG: &str = r##"
brightness = 60

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
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start(calibration_path: &std::path::Path) -> Self {
        let config = v1::Config::parse(CONFIG).expect("test config should parse");
        let workspace = Workspace::from_v1(&config);
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
                preview: galdeck_daemon::preview::Preview::new(),
                widget_host,
                plugin_host,
                plugin_rx,
                parked,
                zone_paint: true,
                calibration_path: Some(calibration_path.to_path_buf()),
            },
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

    fn calibration(&self) -> Calibration {
        match self.request(Request::GetCalibration) {
            Response::Calibration(cal) => cal,
            other => panic!("expected a calibration, got {other:?}"),
        }
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

/// A real measurement, copied from a calibrated unit.
///
/// The bands are the point: dividing this boundary four ways gives 208px
/// rows, and the physical keys are 176 -- which is why a calibration measures
/// each row and column rather than trusting the arithmetic. Zone 0 is nudged
/// off the grid by hand, as the wizard leaves it, so its origin deliberately
/// does not land on an 8px boundary.
fn measured_layout() -> Layout {
    let mut measured = Layout::TEMPLATE;
    measured.set_grid(galdeck::layout::Grid {
        bounds: Rect::new(52, 440, 624, 840),
        rows: 4,
        columns: 3,
        bleed_x: 0,
        bleed_y: 0,
    });
    for (row, band) in [(432, 608), (656, 832), (880, 1056), (1104, 1280)]
        .into_iter()
        .enumerate()
    {
        measured
            .set_row(row as u8, Band::new(band.0, band.1))
            .expect("four rows");
    }
    for (column, band) in [(52, 228), (276, 452), (500, 676)].into_iter().enumerate() {
        measured
            .set_column(column as u8, Band::new(band.0, band.1))
            .expect("three columns");
    }
    measured
        .set_zone(0, Rect::new(49, 433, 176, 176))
        .expect("zone 0 exists");
    measured.source = Source::Calibrated;
    measured
}

fn rect(x: u16, y: u16, width: u16, height: u16) -> CalRect {
    CalRect {
        x,
        y,
        width,
        height,
    }
}

#[test]
fn calibration_over_the_protocol() {
    let root = std::env::temp_dir().join(format!("galdeck-cal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");
    let path = root.join("layout.conf");

    // ---- with no file on disk -------------------------------------------
    {
        let harness = Harness::start(&path);
        let cal = harness.calibration();
        assert_eq!(cal.source, CalSource::Template);
        assert!(
            cal.problem.is_some(),
            "a missing file is worth saying out loud, not silently templating"
        );
        assert_eq!(cal.rows, 4);
        assert_eq!(cal.columns, 3);
        assert_eq!(
            cal.zones.len(),
            12,
            "zones are resolved, not left to the UI"
        );
        assert_eq!(cal.panel_width, galdeck::ids::PANEL_WIDTH);
        assert!(!cal.released);
    }

    // ---- a measured calibration, as the wizard would leave it -----------
    //
    // Row 1 is deliberately not where dividing the boundary would put it,
    // which is the whole reason bands exist.
    let mut measured = Layout::TEMPLATE;
    measured.source = Source::Calibrated;
    measured
        .set_row(1, Band::new(700, 900))
        .expect("row 1 exists in a 4-row grid");
    measured
        .set_zone(5, Rect::new(256, 700, 200, 200))
        .expect("zone 5 exists in a 12-zone grid");
    measured.save(&path).expect("saving the measured layout");

    {
        let harness = Harness::start(&path);
        let cal = harness.calibration();
        // File, not Calibrated: the framework's text format does not carry
        // the source, and `from_text` stamps every layout it reads as File.
        // So `Calibrated` only ever describes a layout still in the memory of
        // the process that measured it, and nothing that survives a restart
        // can claim to have been measured rather than typed.
        assert_eq!(cal.source, CalSource::File);
        assert!(cal.problem.is_none());
        assert!(
            cal.zones.iter().any(|z| z.overridden),
            "the override should reach the UI so it can be drawn differently"
        );

        // ---- an edit that only moves the boundary ------------------------
        //
        // The measured row and the overridden zone are the parts a person
        // did not type, and re-sending the grid must not throw them away.
        assert!(matches!(
            harness.request(Request::SetCalibration {
                screen: cal.screen,
                bounds: rect(0, 440, 720, 840),
                rows: cal.rows,
                columns: cal.columns,
                bleed_x: -4,
                bleed_y: -4,
            }),
            Response::Ok
        ));

        let after = harness.calibration();
        assert_eq!(after.bounds.y, 440);
        assert_eq!(after.bleed_x, -4);
        assert!(
            after.zones.iter().any(|z| z.overridden),
            "a boundary nudge discarded a measured zone"
        );
        let reloaded = Layout::load(&path).expect("the edit should have been saved");
        assert_eq!(
            reloaded.row_band(1),
            Some(Band::new(700, 900)),
            "a boundary nudge discarded a measured row"
        );

        // ---- an edit that invalidates the measurements -------------------
        //
        // Changing the row count moves the rows the bands describe, so the
        // framework drops them. That is correct, and worth pinning down so a
        // later change cannot start half-keeping them.
        assert!(matches!(
            harness.request(Request::SetCalibration {
                screen: after.screen,
                bounds: after.bounds,
                rows: 3,
                columns: after.columns,
                bleed_x: after.bleed_x,
                bleed_y: after.bleed_y,
            }),
            Response::Ok
        ));
        let reloaded = Layout::load(&path).expect("still readable");
        assert_eq!(reloaded.rows(), 3);
        assert_eq!(
            reloaded.row_band(1),
            None,
            "bands measured against four rows must not be reused for three"
        );

        // ---- an edit that cannot work ------------------------------------
        let before = harness.calibration();
        let refused = harness.request(Request::SetCalibration {
            screen: before.screen,
            bounds: before.bounds,
            rows: 0,
            columns: before.columns,
            bleed_x: 0,
            bleed_y: 0,
        });
        assert!(
            matches!(refused, Response::Error { .. }),
            "a zero-row grid should be refused, got {refused:?}"
        );
        assert_eq!(
            harness.calibration().rows,
            before.rows,
            "a refused edit must not have been applied"
        );

        // ---- off the panel ----------------------------------------------
        let refused = harness.request(Request::SetCalibration {
            screen: before.screen,
            bounds: rect(0, 440, 720, 4000),
            rows: before.rows,
            columns: before.columns,
            bleed_x: 0,
            bleed_y: 0,
        });
        assert!(
            matches!(refused, Response::Error { .. }),
            "a grid taller than the panel should be refused, got {refused:?}"
        );

        // ---- reload discards nothing of substance -------------------------
        assert!(matches!(
            harness.request(Request::ReloadCalibration),
            Response::Ok
        ));
        assert_eq!(harness.calibration().rows, 3);
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// The thing calibration is for: content that fills the whole keycap.
///
/// A configured key, an animated one and an empty one all have to go through
/// the region path at the measured rectangle. Before this, every one of them
/// was a 160x160 image the firmware placed, which on a 176x176 key left an
/// eight-pixel band of bare panel all the way round.
#[test]
fn a_calibrated_deck_draws_keys_at_their_measured_size() {
    let root = std::env::temp_dir().join(format!("galdeck-fill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");
    let path = root.join("layout.conf");
    measured_layout().save(&path).expect("saving the layout");

    let harness = Harness::start(&path);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ops = harness.deck.ops();
        let regions = ops
            .iter()
            .filter(|op| matches!(op, DeckOp::KeyRegion { .. }))
            .count();
        if regions >= 12 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {regions} of 12 keys were drawn at their zone: {:?}",
            ops.iter()
                .filter(|op| matches!(op, DeckOp::KeyJpeg { .. }))
                .count()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let ops = harness.deck.ops();
    assert!(
        !ops.iter().any(|op| matches!(op, DeckOp::KeyJpeg { .. })),
        "no key should still be going through the firmware's key path: {ops:#?}"
    );
    assert!(
        harness.deck.violations().is_empty(),
        "the firmware would not forgive: {:?}",
        harness.deck.violations()
    );

    // The JPEGs really are the bigger size, not a 160 image in a 176 frame --
    // the fake decodes every one and would have raised a violation, but this
    // says it out loud.
    for op in &ops {
        if let DeckOp::KeyRegion { width, height, .. } = op {
            assert_eq!(
                (*width, *height),
                (176, 176),
                "a key was drawn at {width}x{height}, not its measured size"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// A screen that shows more than the firmware's 384 rows is filled to its
/// measured edge, and a saved change to it shows at once.
///
/// Found on a unit whose glass shows 396 rows: the tiles were laid out on
/// 384, the twelve rows below them kept whatever the firmware had drawn
/// there, and editing the screen in the UI changed nothing on the deck.
#[test]
fn a_calibrated_screen_is_drawn_at_its_measured_size() {
    let root = std::env::temp_dir().join(format!("galdeck-screen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");
    let path = root.join("layout.conf");
    let mut layout = measured_layout();
    layout.screen = Rect::new(0, 0, 720, 396);
    layout.save(&path).expect("saving the layout");

    let harness = Harness::start(&path);
    let wait_for = |what: &str, found: &dyn Fn(&DeckOp) -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !harness.deck.ops().iter().any(found) {
            assert!(
                std::time::Instant::now() < deadline,
                "{what} never arrived: {:#?}",
                harness.deck.ops()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    // The fake records every panel write the same way, not knowing whose it
    // is; no key sits at the origin.
    wait_for("the screen at 720x400", &|op| {
        matches!(
            op,
            DeckOp::KeyRegion {
                x: 0,
                y: 0,
                width: 720,
                height: 400,
                ..
            }
        )
    });
    assert!(
        !harness
            .deck
            .ops()
            .iter()
            .any(|op| matches!(op, DeckOp::LcdRegion { .. })),
        "the firmware's segment stops at 384 rows"
    );

    // Back to the segment from the UI, without a page switch to repaint.
    harness.deck.clear_ops();
    let cal = harness.calibration();
    assert!(matches!(
        harness.request(Request::SetCalibration {
            screen: rect(0, 0, 720, 384),
            bounds: cal.bounds,
            rows: cal.rows,
            columns: cal.columns,
            bleed_x: cal.bleed_x,
            bleed_y: cal.bleed_y,
        }),
        Response::Ok
    ));
    wait_for("the screen through the segment", &|op| {
        matches!(
            op,
            DeckOp::LcdRegion {
                x: 0,
                y: 0,
                width: 720,
                height: 384,
                ..
            }
        )
    });
    assert!(
        harness.deck.violations().is_empty(),
        "the firmware would not forgive: {:?}",
        harness.deck.violations()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A deck with no calibration keeps the cheap path.
///
/// The template's numbers are arithmetic, not measured, and drawing at
/// rectangles nobody checked against hardware is how content ends up half
/// off the keycap.
#[test]
fn an_uncalibrated_deck_keeps_using_the_key_path() {
    let root = std::env::temp_dir().join(format!("galdeck-nocal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");
    let path = root.join("layout.conf");

    let harness = Harness::start(&path);
    assert_eq!(harness.calibration().source, CalSource::Template);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let keys = harness
            .deck
            .ops()
            .iter()
            .filter(|op| matches!(op, DeckOp::KeyJpeg { .. }))
            .count();
        if keys >= 12 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {keys} keys drawn"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !harness
            .deck
            .ops()
            .iter()
            .any(|op| matches!(op, DeckOp::KeyRegion { .. })),
        "nothing should be drawn at a rectangle that was never measured"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The probe that answers whether content can fill a whole key.
///
/// It cannot answer it here -- only the hardware can say whether a region
/// write lands on the key area -- but it can prove the daemon asks a question
/// the firmware would accept: rectangles inside the panel, on whole 8px
/// blocks, with a JPEG that decodes to exactly the size it was sent for. The
/// fake rejects all three the way the module does, quietly, as violations.
#[test]
fn the_zone_probe_draws_every_zone_as_valid_panel_geometry() {
    let root = std::env::temp_dir().join(format!("galdeck-zone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");
    let path = root.join("layout.conf");

    measured_layout().save(&path).expect("saving the layout");

    let harness = Harness::start(&path);
    assert!(matches!(
        harness.request(Request::ZonePattern),
        Response::Ok
    ));

    // The io thread writes on its own schedule, so wait for the ops rather
    // than assuming they have landed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let regions = harness
            .deck
            .ops()
            .iter()
            .filter(|op| matches!(op, DeckOp::KeyRegion { .. }))
            .count();
        if regions >= 12 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {regions} of 12 zones were drawn"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        harness.deck.violations().is_empty(),
        "the firmware would not forgive: {:?}",
        harness.deck.violations()
    );

    // Every zone, at the rectangle the calibration measured -- including the
    // hand-nudged one, whose origin is deliberately not block-aligned.
    let rects: Vec<(u16, u16, u16, u16)> = harness
        .deck
        .ops()
        .iter()
        .filter_map(|op| match op {
            DeckOp::KeyRegion {
                x,
                y,
                width,
                height,
                ..
            } => Some((*x, *y, *width, *height)),
            _ => None,
        })
        .collect();
    assert!(
        rects.contains(&(49, 433, 176, 176)),
        "the overridden zone should be drawn where it was measured: {rects:?}"
    );
    assert!(
        rects.iter().all(|(_, _, w, h)| *w == 176 && *h == 176),
        "every zone should fill its measured 176x176, not the 208 the bare \
         grid would derive or the 160 the key path can reach: {rects:?}"
    );
    // Three of the four rows sit below y=448, which is as far down the panel
    // as the framework has ever confirmed a region write renders, and the
    // bottom row runs to the very last pixel of the reported height. Whether
    // they light up is the hardware's answer to give; that they are asked
    // for, with geometry the firmware would accept, is this test's.
    assert!(
        rects.iter().filter(|(_, y, _, _)| *y > 448).count() >= 9,
        "the lower rows should be addressed too: {rects:?}"
    );
    assert!(
        rects.iter().any(|(_, y, _, h)| *y + *h == 1280),
        "the bottom row should reach the bottom of the panel: {rects:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
