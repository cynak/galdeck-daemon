//! The fake deck is the foundation every later hardware-free test stands on,
//! so it gets tested itself.

use std::time::Duration;

use galdeck::{Buttons, Canvas, Encoders, Event, Lcd, Rgb, Ring};
use galdeck_core::{Clock, ManualClock, Tick};
use galdeck_device::{Deck, DeckOp, FakeDeck, KeySurface, Violation, FEATURE_REPORT_COST};

/// A real JPEG of the right size. The fake checks dimensions, so tests have
/// to hand it something the firmware would actually accept.
fn sized_jpeg(width: u32, height: u32, shade: u8) -> Vec<u8> {
    Canvas::filled(width, height, Rgb::new(shade, shade, shade))
        .to_jpeg(90)
        .expect("encoding a flat canvas cannot fail")
}

fn jpeg(marker: u8) -> Vec<u8> {
    sized_jpeg(Buttons::PIXEL_SIZE, Buttons::PIXEL_SIZE, marker)
}

#[test]
fn records_what_it_was_told_to_draw() {
    let (mut deck, handle) = FakeDeck::new();
    deck.set_brightness(70).unwrap();
    deck.set_key_jpeg(4, &jpeg(1)).unwrap();
    deck.set_key_color(5, Rgb::new(1, 2, 3)).unwrap();
    deck.clear_key(6).unwrap();
    deck.set_ring_segment(1, 2, Rgb::new(9, 9, 9)).unwrap();

    let surface = handle.surface();
    assert_eq!(surface.brightness, 70);
    assert_eq!(surface.key(4), &KeySurface::Jpeg(jpeg(1).into()));
    assert_eq!(surface.key(5), &KeySurface::Color(Rgb::new(1, 2, 3)));
    assert_eq!(surface.key(6), &KeySurface::Blank);
    assert_eq!(surface.ring(1)[2], Rgb::new(9, 9, 9));
}

#[test]
fn counts_the_two_write_classes_apart() {
    // The classes cost an order of magnitude apart on real hardware -- a
    // feature report forces a 2 ms sleep, an image upload does not -- so
    // dirty-tracking tests need to tell them apart.
    let (mut deck, handle) = FakeDeck::new();
    deck.set_key_jpeg(0, &jpeg(1)).unwrap();
    deck.set_key_color(1, Rgb::WHITE).unwrap();
    deck.draw_lcd_jpeg(0, 0, 100, 100, &sized_jpeg(100, 100, 2))
        .unwrap();

    let surface = handle.surface();
    assert_eq!(surface.image_writes, 2);
    assert_eq!(surface.feature_writes, 1);
    assert_eq!(surface.writes(), 3);
}

#[test]
fn a_solid_key_costs_more_than_the_same_key_as_a_jpeg() {
    // The counter-intuitive fact the whole render design turns on: a solid
    // fill is one feature report and therefore a forced 2 ms sleep, while a
    // small JPEG is a couple of unpaced writes.
    let solid = DeckOp::KeyColor {
        key: 0,
        color: Rgb::WHITE,
    };
    let image = DeckOp::KeyJpeg {
        key: 0,
        // A flat 160x160 key encodes to about a kilobyte.
        jpeg: vec![0u8; 1024].into(),
    };
    assert_eq!(solid.cost(), FEATURE_REPORT_COST);
    assert!(
        image.cost() < solid.cost(),
        "jpeg {:?} should beat solid {:?}",
        image.cost(),
        solid.cost()
    );
}

#[test]
fn a_full_ring_frame_costs_eight_milliseconds() {
    // Four segments per ring, two rings, 2 ms each. This is what caps ring
    // animation at roughly 30 Hz once diffed.
    let one_ring: Duration = (0..Ring::SEGMENTS)
        .map(|segment| {
            DeckOp::RingSegment {
                encoder: 0,
                segment,
                color: Rgb::WHITE,
            }
            .cost()
        })
        .sum();
    assert_eq!(one_ring, Duration::from_millis(8));
}

#[test]
fn a_full_lcd_frame_drops_the_patches_it_hides() {
    let (mut deck, handle) = FakeDeck::new();
    deck.draw_lcd_jpeg(10, 10, 40, 40, &sized_jpeg(40, 40, 1))
        .unwrap();
    deck.draw_lcd_jpeg(60, 10, 40, 40, &sized_jpeg(40, 40, 2))
        .unwrap();
    assert_eq!(handle.surface().lcd.len(), 2);

    deck.draw_lcd_jpeg(
        0,
        0,
        Lcd::WIDTH,
        Lcd::HEIGHT,
        &sized_jpeg(Lcd::WIDTH.into(), Lcd::HEIGHT.into(), 3),
    )
    .unwrap();
    let surface = handle.surface();
    assert_eq!(surface.lcd.len(), 1);
    assert!(surface.lcd[0].is_full_frame());
    // The writes still happened even though the history was collapsed.
    assert_eq!(surface.image_writes, 3);
}

#[test]
fn rejects_geometry_the_hardware_would_reject() {
    let (mut deck, _handle) = FakeDeck::new();
    assert!(deck.set_key_jpeg(Buttons::COUNT, &jpeg(1)).is_err());
    assert!(deck
        .set_ring_segment(Encoders::COUNT, 0, Rgb::WHITE)
        .is_err());
    assert!(deck
        .set_ring_segment(0, Ring::SEGMENTS, Rgb::WHITE)
        .is_err());
    assert!(deck.set_brightness(101).is_err());
    // Rectangle runs off the right edge.
    assert!(deck
        .draw_lcd_jpeg(Lcd::WIDTH - 10, 0, 20, 20, &jpeg(1))
        .is_err());
    // Zero-sized rectangle.
    assert!(deck.draw_lcd_jpeg(0, 0, 0, 10, &jpeg(1)).is_err());
    assert!(deck
        .draw_lcd_jpeg(
            0,
            0,
            Lcd::WIDTH,
            Lcd::HEIGHT,
            &sized_jpeg(Lcd::WIDTH.into(), Lcd::HEIGHT.into(), 1)
        )
        .is_ok());
}

#[test]
fn delivers_injected_input_and_times_out_cleanly() {
    let (mut deck, handle) = FakeDeck::new();
    handle.press(Event::KeyDown(3));
    handle.press(Event::KeyUp(3));

    // Both queued events arrive together, as one input report can decode to
    // several.
    let events = deck.poll(Duration::from_millis(50)).unwrap();
    assert_eq!(events, vec![Event::KeyDown(3), Event::KeyUp(3)]);

    // Nothing queued: the poll returns empty rather than blocking forever.
    let events = deck.poll(Duration::from_millis(10)).unwrap();
    assert!(events.is_empty());
}

#[test]
fn mode_reentry_fires_once_then_clears() {
    let (mut deck, handle) = FakeDeck::new();
    assert!(!deck.take_mode_reentry());
    handle.signal_mode_reentry();
    assert!(deck.take_mode_reentry());
    assert!(!deck.take_mode_reentry());
}

#[test]
fn clear_all_blanks_every_surface() {
    let (mut deck, handle) = FakeDeck::new();
    deck.set_key_jpeg(0, &jpeg(1)).unwrap();
    deck.set_ring_segment(0, 0, Rgb::WHITE).unwrap();
    deck.draw_lcd_jpeg(0, 0, 20, 20, &sized_jpeg(20, 20, 2))
        .unwrap();

    deck.clear_all().unwrap();
    let surface = handle.surface();
    assert!(surface.keys.iter().all(|k| k == &KeySurface::Blank));
    assert!(surface.rings.iter().all(|r| r == &[Rgb::BLACK; 4]));
    assert!(surface.lcd.is_empty());
}

#[test]
fn the_op_stream_is_replayable() {
    // The P0 refactor proof compares op streams byte for byte, so ops must
    // record faithfully and replay onto another deck identically.
    let (mut deck, handle) = FakeDeck::new();
    deck.set_brightness(60).unwrap();
    deck.set_key_jpeg(0, &jpeg(7)).unwrap();
    deck.set_ring_segment(1, 3, Rgb::new(4, 5, 6)).unwrap();
    let recorded = handle.ops();
    assert_eq!(recorded.len(), 3);

    let (mut replay, replay_handle) = FakeDeck::new();
    for op in &recorded {
        op.apply(&mut replay).unwrap();
    }
    assert_eq!(replay_handle.ops(), recorded);
    assert_eq!(replay_handle.surface().brightness, 60);
    assert_eq!(replay_handle.surface().key(0), handle.surface().key(0));
}

#[test]
fn a_jpeg_of_the_wrong_size_is_recorded_as_a_violation() {
    // The framework never checks this: `lcd_region_reports` documents that the
    // JPEG "must decode to exactly width x height" and then only validates the
    // rectangle, and `key_image_reports` never checks 160x160 at all. The
    // firmware's behaviour when they disagree is undefined, so catching it
    // here is the main reason this fake beats a stub.
    let (mut deck, handle) = FakeDeck::new();
    deck.set_key_jpeg(0, &sized_jpeg(80, 80, 1)).unwrap();

    let violations = handle.violations();
    assert_eq!(violations.len(), 1);
    assert!(
        matches!(
            &violations[0],
            Violation::JpegDimensionMismatch {
                expected: (160, 160),
                actual: (80, 80),
                ..
            }
        ),
        "got {violations:?}"
    );
}

#[test]
fn an_lcd_patch_that_does_not_match_its_rectangle_is_caught() {
    let (mut deck, handle) = FakeDeck::new();
    deck.draw_lcd_jpeg(0, 0, 100, 50, &sized_jpeg(100, 100, 1))
        .unwrap();
    assert!(matches!(
        handle.violations().first(),
        Some(Violation::JpegDimensionMismatch {
            expected: (100, 50),
            actual: (100, 100),
            ..
        })
    ));
}

#[test]
fn a_correctly_sized_jpeg_raises_nothing() {
    let (mut deck, handle) = FakeDeck::new();
    deck.set_key_jpeg(0, &jpeg(1)).unwrap();
    deck.draw_lcd_jpeg(4, 4, 64, 32, &sized_jpeg(64, 32, 2))
        .unwrap();
    assert!(handle.violations().is_empty(), "{:?}", handle.violations());
}

#[test]
fn simulated_time_charges_each_write_its_modelled_cost() {
    let clock = std::sync::Arc::new(ManualClock::new());
    let (mut deck, _handle) = FakeDeck::simulated(clock.clone());

    // Four ring segments: four feature reports, 2 ms each.
    for segment in 0..Ring::SEGMENTS {
        deck.set_ring_segment(0, segment, Rgb::WHITE).unwrap();
    }
    assert_eq!(
        clock.now(),
        Tick(8_000),
        "a full ring frame is 8 ms of forced sleeps"
    );
}

#[test]
fn an_empty_poll_passes_simulated_time_without_blocking() {
    let clock = std::sync::Arc::new(ManualClock::new());
    let (mut deck, _handle) = FakeDeck::simulated(clock.clone());

    let started = std::time::Instant::now();
    let events = deck.poll(Duration::from_secs(1)).unwrap();
    assert!(events.is_empty());
    assert_eq!(clock.now(), Tick(1_000_000), "a second passed, in theory");
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "but not in practice"
    );
}

#[test]
fn going_quiet_for_two_seconds_is_reported_and_costs_a_second() {
    // This is the trap the real firmware sets: stop touching the module for
    // two seconds and the next call blocks a full second re-entering software
    // mode, with the ring LEDs wiped white on the way back in. A daemon whose
    // loop can go quiet has this bug and no test would otherwise notice.
    let clock = std::sync::Arc::new(ManualClock::new());
    let (mut deck, handle) = FakeDeck::simulated(clock.clone());
    deck.set_brightness(60).unwrap();

    clock.advance(Duration::from_secs(3));
    deck.set_brightness(61).unwrap();

    let violations = handle.violations();
    assert!(
        matches!(violations.first(), Some(Violation::KeepaliveGap { .. })),
        "got {violations:?}"
    );
    assert!(
        deck.take_mode_reentry(),
        "re-entry wipes ring state, so the caller must be told to repaint"
    );
    // 60 ms of writes plus the 1000 ms settle the framework sleeps for.
    assert!(clock.now() >= Tick(4_000_000), "at {:?}", clock.now());
}

#[test]
fn a_busy_loop_never_trips_the_keepalive_gap() {
    let clock = std::sync::Arc::new(ManualClock::new());
    let (mut deck, handle) = FakeDeck::simulated(clock.clone());
    // Sixty seconds of a 200 ms poll loop, which is what the daemon does when
    // idle. Runs instantly because the time is simulated.
    for _ in 0..300 {
        deck.poll(Duration::from_millis(200)).unwrap();
    }
    assert!(clock.now() >= Tick(60_000_000));
    assert!(handle.violations().is_empty(), "{:?}", handle.violations());
}

#[test]
fn clear_all_costs_more_than_its_feature_reports_suggest() {
    // 12 key fills plus 8 ring LEDs is 40 ms of forced sleeps, but the
    // framework also encodes and uploads a full black 720x384 frame, and that
    // is most of the remaining cost. Under-reporting it would let the io
    // budget schedule a clear it cannot afford.
    let feature_only = FEATURE_REPORT_COST * 20;
    assert!(
        DeckOp::ClearAll.cost() > feature_only + Duration::from_millis(9),
        "clear_all costs {:?}, which does not account for the LCD frame",
        DeckOp::ClearAll.cost()
    );
}
