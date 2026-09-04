//! Diffing is what turns a ~50 ms page repaint into a ~1.4 ms one, so what it
//! chooses not to write matters as much as what it does.

use std::sync::Arc;

use galdeck::{Buttons, Encoders, Lcd, Rgb, Ring};
use galdeck_device::{DeckOp, DeckShadow, KeyTarget, Paint};

fn jpeg(byte: u8) -> Arc<[u8]> {
    Arc::from(vec![byte; 64])
}

fn ring(color: Rgb) -> [Rgb; Ring::SEGMENTS as usize] {
    [color; Ring::SEGMENTS as usize]
}

/// Apply a paint the way the io pump does: plan, write, record.
fn apply(shadow: &mut DeckShadow, paint: &Paint) -> Vec<DeckOp> {
    let ops = shadow.plan(paint);
    for op in &ops {
        shadow.record(op);
    }
    ops
}

#[test]
fn an_unknown_surface_is_always_written() {
    let mut shadow = DeckShadow::new();
    let ops = apply(
        &mut shadow,
        &Paint::Key {
            index: 3,
            target: KeyTarget::Jpeg(jpeg(1)),
        },
    );
    assert_eq!(ops.len(), 1);
}

#[test]
fn painting_the_same_thing_twice_writes_once() {
    let mut shadow = DeckShadow::new();
    let paint = Paint::Key {
        index: 3,
        target: KeyTarget::Jpeg(jpeg(1)),
    };
    assert_eq!(apply(&mut shadow, &paint).len(), 1);
    assert!(
        apply(&mut shadow, &paint).is_empty(),
        "an unchanged key must cost nothing"
    );
}

#[test]
fn an_identical_frame_from_a_different_allocation_still_matches() {
    // The frame cache usually hands back the same Arc, but a re-render
    // produces equal bytes at a new address. Both must count as unchanged.
    let mut shadow = DeckShadow::new();
    apply(
        &mut shadow,
        &Paint::Lcd {
            jpeg: Arc::from(vec![7u8; 512]),
        },
    );
    let ops = apply(
        &mut shadow,
        &Paint::Lcd {
            jpeg: Arc::from(vec![7u8; 512]),
        },
    );
    assert!(ops.is_empty());
}

#[test]
fn a_page_switch_pushes_only_what_changed() {
    // The headline number: twelve keys, both rings and the LCD, of which one
    // key differs on the second page.
    let mut shadow = DeckShadow::new();
    let first: Vec<Paint> = (0..Buttons::COUNT)
        .map(|index| Paint::Key {
            index,
            target: KeyTarget::Jpeg(jpeg(index)),
        })
        .chain((0..Encoders::COUNT).map(|encoder| Paint::Ring {
            encoder,
            colors: ring(Rgb::new(0, 200, 150)),
        }))
        .chain([Paint::Lcd { jpeg: jpeg(99) }])
        .collect();

    let cold: usize = first.iter().map(|p| apply(&mut shadow, p).len()).sum();
    assert_eq!(cold, 12 + 8 + 1, "a cold paint writes everything");

    // Same page again, except key 5.
    let mut second = first.clone();
    second[5] = Paint::Key {
        index: 5,
        target: KeyTarget::Jpeg(jpeg(200)),
    };
    let warm: usize = second.iter().map(|p| apply(&mut shadow, p).len()).sum();
    assert_eq!(warm, 1, "only the key that changed is written");
}

#[test]
fn a_ring_turn_writes_two_segments_not_eight() {
    // A detent moves the lit segment: one goes back to base, one lights up.
    // Writing all four would cost 8 ms instead of 4.
    let mut shadow = DeckShadow::new();
    let base = Rgb::new(0, 200, 150);
    apply(
        &mut shadow,
        &Paint::Ring {
            encoder: 0,
            colors: ring(base),
        },
    );

    let mut turned = ring(base);
    turned[1] = Rgb::WHITE;
    let ops = apply(
        &mut shadow,
        &Paint::Ring {
            encoder: 0,
            colors: turned,
        },
    );
    assert_eq!(ops.len(), 1);

    let mut moved_on = ring(base);
    moved_on[2] = Rgb::WHITE;
    let ops = apply(
        &mut shadow,
        &Paint::Ring {
            encoder: 0,
            colors: moved_on,
        },
    );
    assert_eq!(ops.len(), 2, "one segment darkens, one lights");
}

#[test]
fn forgetting_makes_the_next_paint_unconditional() {
    // The firmware wipes the ring LEDs white when it re-enters software mode.
    // Diffing against a stale mirror would write nothing at all.
    let mut shadow = DeckShadow::new();
    let paint = Paint::Ring {
        encoder: 1,
        colors: ring(Rgb::new(48, 112, 255)),
    };
    apply(&mut shadow, &paint);
    assert!(apply(&mut shadow, &paint).is_empty());

    shadow.forget_everything();
    assert_eq!(
        apply(&mut shadow, &paint).len(),
        Ring::SEGMENTS as usize,
        "every segment must be rewritten after a wipe"
    );
}

#[test]
fn a_partial_lcd_write_does_not_claim_the_whole_panel() {
    // Recording a patch as though it were the frame would make the next full
    // frame a no-op and leave the rest of the panel stale.
    let mut shadow = DeckShadow::new();
    let frame = jpeg(5);
    apply(
        &mut shadow,
        &Paint::Lcd {
            jpeg: frame.clone(),
        },
    );

    shadow.record(&DeckOp::LcdRegion {
        x: 10,
        y: 10,
        width: 40,
        height: 40,
        jpeg: jpeg(6),
    });

    let ops = apply(&mut shadow, &Paint::Lcd { jpeg: frame });
    assert_eq!(ops.len(), 1, "the full frame must be rewritten");
}

#[test]
fn clear_all_is_reflected_so_the_next_paint_is_not_skipped() {
    let mut shadow = DeckShadow::new();
    apply(
        &mut shadow,
        &Paint::Key {
            index: 0,
            target: KeyTarget::Blank,
        },
    );
    shadow.record(&DeckOp::ClearAll);
    // Everything is blank, so painting blank again is genuinely free.
    assert!(shadow
        .plan(&Paint::Key {
            index: 0,
            target: KeyTarget::Blank
        })
        .is_empty());
    // But brightness and the LCD are unknown again.
    assert_eq!(shadow.plan(&Paint::Brightness(60)).len(), 1);
    assert_eq!(shadow.plan(&Paint::Lcd { jpeg: jpeg(1) }).len(), 1);
}

#[test]
fn a_full_frame_write_is_recorded_as_the_whole_panel() {
    let mut shadow = DeckShadow::new();
    let frame = jpeg(3);
    shadow.record(&DeckOp::LcdRegion {
        x: 0,
        y: 0,
        width: Lcd::WIDTH,
        height: Lcd::HEIGHT,
        jpeg: frame.clone(),
    });
    assert!(shadow.plan(&Paint::Lcd { jpeg: frame }).is_empty());
}
