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

use std::time::Duration;

use galdeck_core::Tick;

use galdeck::{Rgb, Ring};

const SEGMENTS: usize = Ring::SEGMENTS as usize;

/// How long the lit segment stays after the last detent.
const TURN_HOLD: Duration = Duration::from_millis(350);
/// How long a level stays on the ring after the last change.
const LEVEL_HOLD: Duration = Duration::from_millis(1500);
/// The colour a muted level is shown in.
const MUTED: Rgb = Rgb::new(191, 97, 106);

/// Where a level ring rests while what it turns is muted: red, but dim
/// enough that a level shown over it still reads.
pub fn muted_rest(base: Rgb) -> Rgb {
    MUTED.lerp(base, 0.55)
}
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
    /// What the hardware is currently showing, per segment. `None` means
    /// unknown -- nothing has been written since the last time the firmware
    /// wiped the ring -- which forces the next diff to repaint that segment.
    shown: [Option<Rgb>; SEGMENTS],
    /// What it should be showing.
    want: [Rgb; SEGMENTS],
    /// Lit segment, stepped one place per detent.
    cursor: u8,
    /// When the current animation returns to rest; `None` when resting.
    until: Option<Tick>,
}

impl RingFeedback {
    pub fn new() -> Self {
        RingFeedback {
            base: Rgb::BLACK,
            shown: [None; SEGMENTS],
            want: [Rgb::BLACK; SEGMENTS],
            cursor: 0,
            until: None,
        }
    }

    /// Record the colour the ring should rest at.
    ///
    /// This deliberately does *not* touch `shown`. `shown` means "what the
    /// hardware is displaying", and only an actual write may claim that.
    /// Setting it here used to be harmless because the page apply always
    /// wrote every segment immediately beforehand; once diffing is the only
    /// write path, that assumption silently loses every repaint after the
    /// firmware wipes the ring.
    pub fn rest(&mut self, base: Rgb) {
        self.base = base;
        self.want = [base; SEGMENTS];
        self.until = None;
    }

    /// Record that all four segments were just written to `base`.
    ///
    /// The page apply writes the whole ring itself, so it may legitimately
    /// claim `shown` -- that is what keeps a page switch from immediately
    /// rewriting the same four segments and spending 8 ms doing it.
    pub fn painted(&mut self, base: Rgb) {
        self.rest(base);
        self.shown = [Some(base); SEGMENTS];
    }

    /// Forget what the hardware is showing, so the next diff repaints
    /// everything.
    ///
    /// The firmware turns all the ring LEDs white when it re-enters software
    /// mode, and says so only through `take_mode_reentry`. Without this, the
    /// rings stay wrong until the next page switch.
    pub fn forget(&mut self) {
        self.shown = [None; SEGMENTS];
    }

    /// One detent: step the lit segment one place in the turn's direction.
    /// Segments are numbered clockwise from the top, so a clockwise turn
    /// counts up.
    pub fn turn(&mut self, delta: i8, now: Tick) {
        let step = if delta > 0 { 1 } else { SEGMENTS - 1 };
        self.cursor = ((self.cursor as usize + step) % SEGMENTS) as u8;
        self.want = [self.base; SEGMENTS];
        self.want[self.cursor as usize] = self.highlight();
        self.until = Some(now.saturating_add(TURN_HOLD));
    }

    /// Show a level, 0 to 1, as a bar round the ring: whole segments lit,
    /// and the next one lit in proportion to the remainder, so every step of
    /// a four-segment ring is visible and not only every quarter.
    ///
    /// Muted, the lit part is red and dim, so a muted output is obvious at a
    /// glance and never mistaken for a quiet one.
    pub fn level(&mut self, fraction: f32, muted: bool, now: Tick) {
        let fraction = fraction.clamp(0.0, 1.0);
        let lit = if muted {
            MUTED.lerp(self.base, 0.4)
        } else {
            self.highlight()
        };
        for (segment, want) in self.want.iter_mut().enumerate() {
            let share = (fraction * SEGMENTS as f32 - segment as f32).clamp(0.0, 1.0);
            *want = self.base.lerp(lit, share);
        }
        // Muted at zero would otherwise be indistinguishable from off.
        if muted && fraction == 0.0 {
            self.want = [MUTED.lerp(self.base, 0.7); SEGMENTS];
        }
        self.until = Some(now.saturating_add(LEVEL_HOLD));
    }

    /// Show a position among `count` (a page among the profile's pages): the
    /// segment for it lit, counting clockwise from the top and wrapping.
    pub fn position(&mut self, index: usize, now: Tick) {
        self.want = [self.base; SEGMENTS];
        self.want[index % SEGMENTS] = self.highlight();
        self.until = Some(now.saturating_add(LEVEL_HOLD));
    }

    /// A click: light the whole ring.
    pub fn click(&mut self, now: Tick) {
        self.want = [self.highlight(); SEGMENTS];
        self.until = Some(now.saturating_add(CLICK_HOLD));
    }

    /// The segments needing a repaint, as `(segment, colour)`. Returns the
    /// changed ones only, and assumes the caller writes every one it gets.
    pub fn updates(&mut self, now: Tick) -> Vec<(u8, Rgb)> {
        self.expire(now);
        let mut updates = Vec::new();
        for segment in 0..SEGMENTS {
            if self.shown[segment] != Some(self.want[segment]) {
                self.shown[segment] = Some(self.want[segment]);
                updates.push((segment as u8, self.want[segment]));
            }
        }
        updates
    }

    /// Drop back to rest if the hold has run out.
    ///
    /// Separate from `updates` because the diffing now happens in the device
    /// mirror; this is the part that is still the ring's own business.
    pub fn expire(&mut self, now: Tick) {
        if self.until.is_some_and(|until| now >= until) {
            self.want = [self.base; SEGMENTS];
            self.until = None;
        }
    }

    /// When the ring owes itself a return to rest, if it does.
    ///
    /// The core loop registers this as a deadline rather than polling for it,
    /// which is what lets an idle daemon cost nothing.
    pub fn deadline(&self) -> Option<Tick> {
        self.until
    }

    /// The colours the ring should currently be showing.
    pub fn colors(&self) -> [Rgb; SEGMENTS] {
        self.want
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

    fn tick(ms: u64) -> Tick {
        Tick(ms * 1000)
    }

    #[test]
    fn a_level_lights_whole_segments_and_part_of_the_next() {
        let mut ring = RingFeedback::new();
        ring.rest(Rgb::BLACK);
        ring.level(0.375, false, tick(0));
        let colors = ring.colors();
        let full = Rgb::BLACK.lerp(Rgb::WHITE, HIGHLIGHT);
        assert_eq!(colors[0], full);
        // Half of the second segment: 0.375 * 4 = 1.5.
        assert_eq!(colors[1], Rgb::BLACK.lerp(full, 0.5));
        assert_eq!(colors[2], Rgb::BLACK);
        assert_eq!(colors[3], Rgb::BLACK);
        assert!(ring.is_active());
        ring.expire(tick(LEVEL_HOLD.as_millis() as u64 + 1));
        assert!(!ring.is_active());
    }

    #[test]
    fn muted_is_red_even_at_zero() {
        let mut ring = RingFeedback::new();
        ring.rest(Rgb::BLACK);
        ring.level(0.0, true, tick(0));
        assert!(ring.colors().iter().all(|c| c.r > c.g && c.r > 0));
    }

    #[test]
    fn a_position_lights_its_segment() {
        let mut ring = RingFeedback::new();
        ring.rest(Rgb::BLACK);
        ring.position(5, tick(0));
        let lit: Vec<usize> = (0..SEGMENTS)
            .filter(|i| ring.colors()[*i] != Rgb::BLACK)
            .collect();
        assert_eq!(lit, vec![1]);
    }

    const BASE: Rgb = Rgb::new(0, 200, 150);

    fn rested() -> (RingFeedback, Tick) {
        let mut ring = RingFeedback::new();
        ring.painted(BASE);
        (ring, Tick(1_000_000))
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
        assert!(ring.updates(now.saturating_add(TURN_HOLD / 2)).is_empty());
        assert!(ring.is_active());

        assert_eq!(ring.updates(now.saturating_add(TURN_HOLD)), vec![(1, BASE)]);
        assert!(!ring.is_active());
    }

    #[test]
    fn a_click_lights_the_whole_ring_then_clears() {
        let (mut ring, now) = rested();
        ring.click(now);
        assert_eq!(ring.updates(now).len(), SEGMENTS);

        let cleared = ring.updates(now.saturating_add(CLICK_HOLD));
        assert_eq!(cleared.len(), SEGMENTS);
        assert!(cleared.iter().all(|(_, color)| *color == BASE));
    }

    #[test]
    fn an_unconfigured_black_ring_still_lights_up() {
        let mut ring = RingFeedback::new();
        ring.painted(Rgb::BLACK);
        let now = Tick(1_000_000);
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

        ring.painted(Rgb::BLUE);
        assert!(!ring.is_active());
        assert!(ring.updates(now).is_empty());
    }

    #[test]
    fn forgetting_repaints_all_four_segments() {
        // The firmware turns every ring LED white when it re-enters software
        // mode. Diffing against a stale `shown` would then produce no writes
        // at all and leave the rings wrong until the next page switch.
        let (mut ring, now) = rested();
        assert!(ring.updates(now).is_empty());

        ring.forget();
        let updates = ring.updates(now);
        assert_eq!(updates.len(), SEGMENTS, "every segment must be rewritten");
        assert!(updates.iter().all(|(_, color)| *color == BASE));
    }

    #[test]
    fn resting_alone_does_not_claim_the_hardware_shows_it() {
        // `rest` records intent; only a real write may claim `shown`. A ring
        // that has never been painted must still emit its first four writes.
        let mut ring = RingFeedback::new();
        ring.rest(BASE);
        let updates = ring.updates(Tick(1_000_000));
        assert_eq!(updates.len(), SEGMENTS);
    }
}
