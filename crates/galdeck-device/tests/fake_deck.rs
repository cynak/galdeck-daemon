//! The fake deck is the foundation every later hardware-free test stands on,
//! so it gets tested itself.

use std::time::Duration;

use galdeck::{Buttons, Encoders, Event, Lcd, Rgb, Ring};
use galdeck_device::{Deck, DeckOp, FakeDeck, KeySurface, FEATURE_REPORT_COST};

fn jpeg(marker: u8) -> Vec<u8> {
    vec![0xff, 0xd8, marker, 0xff, 0xd9]
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
    deck.draw_lcd_jpeg(0, 0, 100, 100, &jpeg(2)).unwrap();

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
    deck.draw_lcd_jpeg(10, 10, 40, 40, &jpeg(1)).unwrap();
    deck.draw_lcd_jpeg(60, 10, 40, 40, &jpeg(2)).unwrap();
    assert_eq!(handle.surface().lcd.len(), 2);

    deck.draw_lcd_jpeg(0, 0, Lcd::WIDTH, Lcd::HEIGHT, &jpeg(3))
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
        .draw_lcd_jpeg(0, 0, Lcd::WIDTH, Lcd::HEIGHT, &jpeg(1))
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
    deck.draw_lcd_jpeg(0, 0, 20, 20, &jpeg(2)).unwrap();

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
