//! Ring-LED turn feedback.
//!
//! A ring has no idea what its knob is bound to, so it shows the one thing
//! the daemon always knows: that the knob moved. A detent lights a single
//! segment and steps it around the ring in the direction of the turn; a
//! click lights the whole ring. Both settle back to the page's static
//! colour a moment later.
//!
//! Every ring write is a feature report, and this firmware garbles bursts
//! of them (see `Galleon::send_feature`), so this state machine hands back
//! only the segments whose colour actually changed — a detent costs two
//! writes, not four.

use std::time::{Duration, Instant};

use galdeck::{Rgb, Ring};

const SEGMENTS: usize = Ring::SEGMENTS as usize;

/// How long the lit segment stays after the last detent.
const TURN_HOLD: Duration = Duration::from_millis(350);
/// How long the whole ring stays lit after a click.
const CLICK_HOLD: Duration = Duration::from_millis(180);
/// How far the lit colour is blended towards white. A black ring — the
/// default when a page sets no colour — still lights up as dim white, so
/// an unconfigured knob shows feedback too.
const HIGHLIGHT: f32 = 0.7;

/// Turn-feedback state for one encoder's ring.
pub struct RingFeedback {
    /// Colour the page painted the ring; what it rests at.
    base: Rgb,
    /// What the hardware is currently showing, per segment.
    shown: [Rgb; SEGMENTS],
    /// What it should be showing.
    want: [Rgb; SEGMENTS],
    /// Lit segment, stepped one place per detent.
    cursor: u8,
    /// When the current animation returns to rest; `None` when resting.
    until: Option<Instant>,
}

impl RingFeedback {
    pub fn new() -> Self {
        RingFeedback {
            base: Rgb::BLACK,
            shown: [Rgb::BLACK; SEGMENTS],
            want: [Rgb::BLACK; SEGMENTS],
            cursor: 0,
            until: None,
        }
    }

    /// Record the colour the page just painted across the whole ring. The
    /// hardware already shows it, so this yields no updates.
    pub fn rest(&mut self, base: Rgb) {
        self.base = base;
        self.shown = [base; SEGMENTS];
        self.want = [base; SEGMENTS];
        self.until = None;
    }

    /// One detent: step the lit segment one place in the turn's direction.
    /// Segments are numbered clockwise from the top, so a clockwise turn
    /// counts up.
    pub fn turn(&mut self, delta: i8, now: Instant) {
        let step = if delta > 0 { 1 } else { SEGMENTS - 1 };
        self.cursor = ((self.cursor as usize + step) % SEGMENTS) as u8;
        self.want = [self.base; SEGMENTS];
        self.want[self.cursor as usize] = self.highlight();
        self.until = Some(now + TURN_HOLD);
    }

    /// A click: light the whole ring.
    pub fn click(&mut self, now: Instant) {
        self.want = [self.highlight(); SEGMENTS];
        self.until = Some(now + CLICK_HOLD);
    }

    /// The segments needing a repaint, as `(segment, colour)`. Returns the
    /// changed ones only, and assumes the caller writes every one it gets.
    pub fn updates(&mut self, now: Instant) -> Vec<(u8, Rgb)> {
        if self.until.is_some_and(|until| now >= until) {
            self.want = [self.base; SEGMENTS];
            self.until = None;
        }
        let mut updates = Vec::new();
        for segment in 0..SEGMENTS {
            if self.shown[segment] != self.want[segment] {
                self.shown[segment] = self.want[segment];
                updates.push((segment as u8, self.want[segment]));
            }
        }
        updates
    }

    /// True while the ring still owes itself a return to rest. The engine
    /// polls the device on a shorter timeout while this holds, so the
    /// segment goes dark on time rather than at the next input event.
    pub fn is_active(&self) -> bool {
        self.until.is_some()
    }

    fn highlight(&self) -> Rgb {
        self.base.lerp(Rgb::WHITE, HIGHLIGHT)
    }
}

impl Default for RingFeedback {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Rgb = Rgb::new(0, 200, 150);

    fn rested() -> (RingFeedback, Instant) {
        let mut ring = RingFeedback::new();
        ring.rest(BASE);
        (ring, Instant::now())
    }

    #[test]
    fn resting_needs_no_writes() {
        let (mut ring, now) = rested();
        assert!(ring.updates(now).is_empty());
        assert!(!ring.is_active());
    }

    #[test]
    fn a_detent_lights_one_segment() {
        let (mut ring, now) = rested();
        ring.turn(1, now);
        let updates = ring.updates(now);
        // Segment 0 was already at base, so only the newly lit one moves.
        assert_eq!(updates, vec![(1, BASE.lerp(Rgb::WHITE, HIGHLIGHT))]);
        // Nothing changed since, so nothing to rewrite.
        assert!(ring.updates(now).is_empty());
    }

    #[test]
    fn turning_steps_the_cursor_both_ways() {
        let (mut ring, now) = rested();
        let lit = |ring: &mut RingFeedback| {
            ring.updates(now)
                .into_iter()
                .find(|(_, color)| *color != BASE)
                .map(|(segment, _)| segment)
        };

        ring.turn(1, now);
        assert_eq!(lit(&mut ring), Some(1));
        ring.turn(1, now);
        assert_eq!(lit(&mut ring), Some(2));
        // Anticlockwise walks back, and wraps below zero.
        ring.turn(-1, now);
        assert_eq!(lit(&mut ring), Some(1));
        ring.turn(-1, now);
        assert_eq!(lit(&mut ring), Some(0));
        ring.turn(-1, now);
        assert_eq!(lit(&mut ring), Some(SEGMENTS as u8 - 1));
    }

    #[test]
    fn a_detent_costs_two_writes_once_the_cursor_has_moved() {
        let (mut ring, now) = rested();
        ring.turn(1, now);
        ring.updates(now);
        ring.turn(1, now);
        // The segment being left and the one being lit — not all four.
        assert_eq!(ring.updates(now).len(), 2);
    }

    #[test]
    fn the_ring_returns_to_base_after_the_hold() {
        let (mut ring, now) = rested();
        ring.turn(1, now);
        ring.updates(now);
        assert!(ring.is_active());

        // Still lit part way through the hold.
        assert!(ring.updates(now + TURN_HOLD / 2).is_empty());
        assert!(ring.is_active());

        assert_eq!(ring.updates(now + TURN_HOLD), vec![(1, BASE)]);
        assert!(!ring.is_active());
    }

    #[test]
    fn a_click_lights_the_whole_ring_then_clears() {
        let (mut ring, now) = rested();
        ring.click(now);
        assert_eq!(ring.updates(now).len(), SEGMENTS);

        let cleared = ring.updates(now + CLICK_HOLD);
        assert_eq!(cleared.len(), SEGMENTS);
        assert!(cleared.iter().all(|(_, color)| *color == BASE));
    }

    #[test]
    fn an_unconfigured_black_ring_still_lights_up() {
        let mut ring = RingFeedback::new();
        ring.rest(Rgb::BLACK);
        let now = Instant::now();
        ring.turn(1, now);
        let updates = ring.updates(now);
        assert_eq!(updates.len(), 1);
        assert_ne!(updates[0].1, Rgb::BLACK);
    }

    #[test]
    fn applying_a_page_mid_animation_resets_to_the_new_colour() {
        let (mut ring, now) = rested();
        ring.turn(1, now);
        ring.updates(now);

        ring.rest(Rgb::BLUE);
        assert!(!ring.is_active());
        assert!(ring.updates(now).is_empty());
    }
}
