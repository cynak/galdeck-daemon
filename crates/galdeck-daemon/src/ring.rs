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
use galdeck_model::{Animation, AnimationKind};

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

/// Most often an animated ring is redrawn.
///
/// Each redraw is up to four feature reports, 8 ms of the io thread, and the
/// io thread is also what reads the keys. Both rings at this rate keep it
/// about a third busy, which leaves room for key images and input.
pub const ANIMATION_FRAME_MIN: Duration = Duration::from_millis(40);
/// How often an animated ring is redrawn while a knob is being turned.
///
/// The turned knob's feedback writes LEDs of its own, up to two a detent,
/// and with the animation at full rate as well the deck got about twice its
/// resting load, under which its firmware dropped off the bus (3.05.005).
/// Slowed to this, the animation keeps moving and the total stays near rest.
pub const ANIMATION_FRAME_BUSY: Duration = Duration::from_millis(200);
/// How many steps an animation's cycle is cut into, where the rate above
/// allows it. At 120 a rainbow moves 3° of hue a step and a comet's head a
/// thirtieth of a segment, which the eye takes as continuous.
const ANIMATION_STEPS: u32 = 120;

/// How often an animated ring is redrawn.
///
/// A key cycles through the few frames it pre-rendered; a ring computes each
/// frame when it is due, so it is limited only by what the deck can take. A
/// blink has two states and needs only two redraws a cycle.
pub fn animation_interval(animation: &Animation) -> Duration {
    let period = Duration::from_millis(u64::from(animation.period_ms()));
    if animation.kind == AnimationKind::Blink {
        return period / 2;
    }
    (period / ANIMATION_STEPS).max(ANIMATION_FRAME_MIN)
}

/// When the frame after `now` is due, for an animation begun at `start` and
/// redrawn every `interval`: on its own grid of intervals, so a frame that
/// runs late does not push every one after it later too.
pub fn next_animation_frame(start: Tick, interval: Duration, now: Tick) -> Tick {
    let interval = interval.as_micros().max(1);
    let elapsed = now.duration_since(start).as_micros();
    let due = (elapsed / interval + 1) * interval;
    start.saturating_add(Duration::from_micros(due.min(u128::from(u64::MAX)) as u64))
}

/// How far through its cycle, 0 to 1, an animation begun at `start` is at
/// `now`. Measured from the start rather than counted in frames, so a frame
/// that runs late shows where the animation should be by then.
pub fn animation_phase(animation: &Animation, start: Tick, now: Tick) -> f32 {
    let period = u128::from(animation.period_ms()) * 1000;
    let elapsed = now.duration_since(start).as_micros();
    (elapsed % period) as f32 / period as f32
}

/// The colours of an animated ring at `phase` through its cycle, moving from
/// `base` towards `to`.
///
/// Continuous in `phase`, so a frame a little later is a little further on
/// and never a jump: a spin or a comet's head sits partly on each of the two
/// segments it is between, and passes from one to the next as a crossfade.
pub fn animation_frame(kind: AnimationKind, phase: f32, base: Rgb, to: Rgb) -> [Rgb; SEGMENTS] {
    let round = SEGMENTS as f32;
    // Where the head is, in segments clockwise from the top.
    let head = phase.rem_euclid(1.0) * round;
    // How far behind the head a segment is: from -1, the one it is about to
    // reach, to one short of all the way round.
    let behind = |segment: usize| {
        let distance = (head - segment as f32).rem_euclid(round);
        if distance > round - 1.0 {
            distance - round
        } else {
            distance
        }
    };
    match kind {
        // One segment's worth of light going round, shared between the two
        // segments it is between.
        AnimationKind::Spin => {
            std::array::from_fn(|segment| base.lerp(to, 1.0 - behind(segment).abs()))
        }
        // A head that brightens as it arrives, and a tail fading out over the
        // rest of the ring behind it.
        AnimationKind::Comet => std::array::from_fn(|segment| {
            let behind = behind(segment);
            let strength = if behind < 0.0 {
                1.0 + behind
            } else {
                1.0 - behind / (round - 1.0)
            };
            base.lerp(to, strength)
        }),
        // Round the wheel, each segment a quarter turn on from the last, so
        // the colours travel round the knob.
        AnimationKind::Rainbow => std::array::from_fn(|segment| {
            let offset = segment as f32 / round;
            galdeck_model::animation::rainbow_or(kind, phase + offset, base, to)
        }),
        kind => [base.lerp(to, kind.mix_at(phase)); SEGMENTS],
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

    fn animation(kind: AnimationKind, period_ms: u32) -> Animation {
        Animation {
            kind,
            period_ms,
            to: None,
            frames: 8,
        }
    }

    #[test]
    fn ring_animations_never_jump_between_frames() {
        // Stepped a thousandth of a cycle at a time, no channel of any
        // segment may move by more than a few levels. The old comet and spin
        // moved their head a whole segment at once, which is a jump of the
        // whole distance from base to the lit colour.
        let (base, to) = (Rgb::BLACK, Rgb::WHITE);
        for kind in [
            AnimationKind::Spin,
            AnimationKind::Comet,
            AnimationKind::Rainbow,
            AnimationKind::Pulse,
            AnimationKind::Breathe,
        ] {
            let steps = 1000;
            let mut last = animation_frame(kind, 0.0, base, to);
            // Once past the end, to check the wrap back to the start too.
            for step in 1..=steps + 1 {
                let phase = step as f32 / steps as f32;
                let frame = animation_frame(kind, phase, base, to);
                for (segment, (was, now)) in last.iter().zip(frame.iter()).enumerate() {
                    let moved = was
                        .to_array()
                        .iter()
                        .zip(now.to_array())
                        .map(|(a, b)| a.abs_diff(b))
                        .max()
                        .unwrap_or(0);
                    assert!(
                        moved <= 3,
                        "{kind:?} segment {segment} jumped {moved} at phase {phase}"
                    );
                }
                last = frame;
            }
        }
    }

    #[test]
    fn a_comet_between_segments_lights_both() {
        let (base, to) = (Rgb::BLACK, Rgb::WHITE);
        // Head exactly on segment 1: it is fully lit, the tail fades behind
        // it, and the segment it is heading for is still dark.
        let on = animation_frame(AnimationKind::Comet, 0.25, base, to);
        assert_eq!(on[1], to);
        assert_eq!(on[0], base.lerp(to, 2.0 / 3.0));
        assert_eq!(on[3], base.lerp(to, 1.0 / 3.0));
        assert_eq!(on[2], base);

        // Half way to segment 2: segment 2 is half lit as the head arrives,
        // and segment 1 has begun to fade.
        let between = animation_frame(AnimationKind::Comet, 0.375, base, to);
        assert_eq!(between[2], base.lerp(to, 0.5));
        assert_eq!(between[1], base.lerp(to, 1.0 - 0.5 / 3.0));
    }

    #[test]
    fn a_spin_between_segments_shares_its_light() {
        let (base, to) = (Rgb::BLACK, Rgb::WHITE);
        let on = animation_frame(AnimationKind::Spin, 0.5, base, to);
        assert_eq!(on, [base, base, to, base]);

        // A quarter of the way on to segment 3.
        let between = animation_frame(AnimationKind::Spin, 0.5 + 0.125 / 2.0, base, to);
        assert_eq!(between[2], base.lerp(to, 0.75));
        assert_eq!(between[3], base.lerp(to, 0.25));
        assert_eq!(between[0], base);
        assert_eq!(between[1], base);
    }

    #[test]
    fn a_blink_stays_two_colours() {
        let (base, to) = (Rgb::BLACK, Rgb::new(255, 0, 0));
        for step in 0..100 {
            let frame = animation_frame(AnimationKind::Blink, step as f32 / 100.0, base, to);
            assert!(frame.iter().all(|c| *c == base || *c == to));
        }
    }

    #[test]
    fn rings_redraw_often_but_within_what_the_deck_takes() {
        // A two-second rainbow is capped at the frame rate the io thread can
        // spare, which is still fifty steps a cycle.
        let interval = animation_interval(&animation(AnimationKind::Rainbow, 2000));
        assert_eq!(interval, ANIMATION_FRAME_MIN);
        // A slow one is cut into fine steps rather than redrawn needlessly.
        let slow = animation_interval(&animation(AnimationKind::Comet, 60_000));
        assert_eq!(slow, Duration::from_millis(500));
        // A blink has only its two states to show.
        let blink = animation_interval(&animation(AnimationKind::Blink, 400));
        assert_eq!(blink, Duration::from_millis(200));
    }

    #[test]
    fn a_ring_animation_is_timed_from_its_start() {
        let comet = animation(AnimationKind::Comet, 2000);
        let start = tick(500);
        assert_eq!(animation_phase(&comet, start, start), 0.0);
        assert_eq!(animation_phase(&comet, start, tick(1000)), 0.25);
        assert_eq!(animation_phase(&comet, start, tick(2500)), 0.0);

        // A frame that ran late is followed by the next on the grid, not by
        // a whole interval after it.
        let interval = Duration::from_millis(40);
        assert_eq!(next_animation_frame(start, interval, tick(540)), tick(580));
        assert_eq!(next_animation_frame(start, interval, tick(553)), tick(580));
        assert_eq!(next_animation_frame(start, interval, tick(579)), tick(580));
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
