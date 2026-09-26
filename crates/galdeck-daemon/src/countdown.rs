//! Timers and stopwatches.
//!
//! A timer key counts down from its length and a stopwatch key counts up;
//! both are started, paused and reset by hand. This is the state machine
//! behind them, pure over [`Tick`] as `input.rs` is, so a test can walk a
//! 25-minute timer to its end without waiting 25 minutes.
//!
//! Nothing here keeps a clock or schedules anything. The engine asks
//! [`Countdown::end`] when to come back and call [`Countdown::finish_if_due`],
//! and [`Countdown::next_change`] when the key's text next changes, and paints
//! then. A running timer costs one paint a second and a paused one nothing.
//!
//! Ticks come from the monotonic clock, which stops while the machine is
//! suspended, so a suspend pauses every timer along with it.

use std::time::Duration;

use galdeck_core::Tick;

/// How long a finished timer's key flashes before it settles.
///
/// A flash that went on until someone came back could run for hours on a
/// desk nobody is at. Ten seconds is long enough to catch the eye of anyone
/// nearby; after it the key rests in the alarm colour until it is tapped.
pub const FLASH_FOR: Duration = Duration::from_secs(10);

/// Each half of the flash: on for one phase, off for the next.
///
/// One flash a second, well inside the three a second that photosensitivity
/// guidance allows. The phase is worked out from the clock rather than
/// flipped on each paint, because the engine also paints when anything else
/// wakes it, and a knob spun beside a finished timer would otherwise make it
/// strobe.
pub const FLASH_PHASE: Duration = Duration::from_millis(500);

/// The most a stopwatch shows, 99:59:59. Past it the display stops rather
/// than growing a digit the key has no room for.
const STOPWATCH_CAP: u64 = 99 * 3600 + 59 * 60 + 59;

/// What [`Countdown::reset`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reset {
    /// Back at the start, stopped.
    Done,
    /// A running timer, left running. The engine tells the person how to
    /// reset it: "Tap to pause, hold to reset".
    Refused,
}

/// What a timer or stopwatch key shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    /// The time, as `m:ss` below an hour and `h:mm:ss` from an hour.
    pub text: String,
    /// How much of a timer is left, from 1 at the start to 0 at the end, for
    /// a bar or a gauge. `None` for a stopwatch, which has no end to measure
    /// against.
    pub fraction: Option<f32>,
    /// Started and then paused, so the key is drawn dimmed. One that has not
    /// been started yet is not paused: it is ready.
    pub paused: bool,
    /// A timer that has run out.
    pub done: bool,
}

/// One timer or stopwatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Countdown {
    /// How long a timer runs. `None` makes it a stopwatch, which counts up
    /// and never ends.
    length: Option<Duration>,
    /// Time run before the current stretch, added up over every pause.
    ran: Duration,
    /// When the current stretch started, while it is running.
    since: Option<Tick>,
    /// When a timer ran out, once it has. This is the moment it ran out, not
    /// the moment anyone noticed, so the flash keeps the same phase however
    /// late the engine looked.
    done_at: Option<Tick>,
}

impl Countdown {
    /// A timer of `length`, or a stopwatch when that is `None`, stopped at
    /// its start.
    pub fn new(length: Option<Duration>) -> Self {
        Countdown {
            length,
            ran: Duration::ZERO,
            since: None,
            done_at: None,
        }
    }

    /// How long this timer runs, or `None` for a stopwatch. A reload keeps a
    /// countdown only while its key still asks for the same thing.
    pub fn length(&self) -> Option<Duration> {
        self.length
    }

    /// A tap: start, pause or resume. On a timer that has finished, reset it.
    ///
    /// The tap is how someone acknowledges the alarm, and a finished timer
    /// has nothing left to resume, so one tap both stops the alarm and makes
    /// the timer ready to run again.
    ///
    /// Call [`finish_if_due`](Self::finish_if_due) first, so a timer that ran
    /// out a moment ago finishes, and runs its `on_done`, rather than pausing
    /// at 0:00.
    pub fn toggle(&mut self, now: Tick) {
        if self.is_done() {
            *self = Countdown::new(self.length);
        } else if self.running() {
            self.ran = self.elapsed(now);
            self.since = None;
        } else {
            self.since = Some(now);
        }
    }

    /// A hold: back to the start, unless a timer is still counting down.
    ///
    /// A hold takes 400 ms, which a slow tap can reach, and one slow tap must
    /// not wipe out twenty minutes of a running timer. So a running timer
    /// refuses, and it resets once paused or finished. One that has run out
    /// but is not marked done yet has nothing left to lose, so it resets too.
    ///
    /// A stopwatch resets even while it runs. Starting a count over is what
    /// its reset is for, and it has no alarm still to come.
    pub fn reset(&mut self, now: Tick) -> Reset {
        let counting = self.running() && self.remaining(now).is_some_and(|left| !left.is_zero());
        if counting {
            return Reset::Refused;
        }
        *self = Countdown::new(self.length);
        Reset::Done
    }

    /// Whether it is counting.
    pub fn running(&self) -> bool {
        self.since.is_some()
    }

    /// How long a timer has left, or `None` for a stopwatch.
    pub fn remaining(&self, now: Tick) -> Option<Duration> {
        self.length
            .map(|length| length.saturating_sub(self.elapsed(now)))
    }

    /// How long it has run, not counting pauses. A timer never counts past
    /// its length.
    pub fn elapsed(&self, now: Tick) -> Duration {
        let run = match self.since {
            Some(since) => self.ran.saturating_add(now.duration_since(since)),
            None => self.ran,
        };
        match self.length {
            Some(length) => run.min(length),
            None => run,
        }
    }

    /// When a running timer runs out. `None` for a paused, stopped or
    /// finished one, and for a stopwatch, none of which has an end coming.
    pub fn end(&self) -> Option<Tick> {
        let since = self.since?;
        let length = self.length?;
        Some(after(since, length.saturating_sub(self.ran)))
    }

    /// Mark a timer done if it has run out by `now`, and say so the first
    /// time only, so the engine runs its `on_done` once.
    ///
    /// Nobody need be looking: the key's page may not be showing. The done
    /// time recorded is the end itself rather than `now`.
    pub fn finish_if_due(&mut self, now: Tick) -> bool {
        let (Some(end), Some(length)) = (self.end(), self.length) else {
            return false;
        };
        if now < end {
            return false;
        }
        self.done_at = Some(end);
        self.since = None;
        self.ran = length;
        true
    }

    /// Whether a timer has run out and not been tapped since.
    pub fn is_done(&self) -> bool {
        self.done_at.is_some()
    }

    /// What the key shows at `now`.
    pub fn display(&self, now: Tick) -> Display {
        let (secs, fraction) = match (self.remaining(now), self.length) {
            // Rounded up, so a 25-minute timer reads 25:00 for its first
            // second and 0:00 only once it has run out.
            (Some(left), Some(length)) => {
                let secs = left
                    .as_secs()
                    .saturating_add(u64::from(left.subsec_nanos() > 0));
                (secs, Some(fraction_of(left, length)))
            }
            // Rounded down, as a stopwatch shows only whole seconds gone.
            _ => (self.elapsed(now).as_secs().min(STOPWATCH_CAP), None),
        };
        Display {
            text: clock_text(secs),
            fraction,
            paused: !self.running() && !self.is_done() && !self.ran.is_zero(),
            done: self.is_done(),
        }
    }

    /// When the text next changes, while it is running: the next whole
    /// second of the display. `None` when nothing will change without a tap,
    /// including a timer that has already run out and a stopwatch at its cap.
    pub fn next_change(&self, now: Tick) -> Option<Tick> {
        self.since?;
        let wait = match self.remaining(now) {
            Some(left) => {
                if left.is_zero() {
                    return None;
                }
                // The text shows `left` rounded up, so it changes when `left`
                // falls to the whole second below it: 1 s away when `left` is
                // itself whole, less otherwise.
                let below =
                    Duration::from_secs(left.saturating_sub(Duration::from_nanos(1)).as_secs());
                left - below
            }
            None => {
                let gone = self.elapsed(now);
                let secs = gone.as_secs();
                if secs >= STOPWATCH_CAP {
                    return None;
                }
                Duration::from_secs(secs + 1) - gone
            }
        };
        Some(after(now, wait))
    }

    /// Whether a finished timer's key is in the bright half of its flash.
    ///
    /// Every paint in the same phase agrees, however often the engine paints,
    /// so the key changes at most twice a second and never after
    /// [`FLASH_FOR`].
    pub fn flash_on(&self, now: Tick) -> bool {
        let Some(done_at) = self.done_at else {
            return false;
        };
        let since = now.duration_since(done_at);
        let phase = since.as_micros() / FLASH_PHASE.as_micros();
        self.flashing(now) && phase.is_multiple_of(2)
    }

    /// Whether a finished timer is still inside its flash. The engine keeps
    /// its flash timer going while any showing key says yes.
    pub fn flashing(&self, now: Tick) -> bool {
        self.done_at
            .is_some_and(|done_at| now.duration_since(done_at) < FLASH_FOR)
    }
}

/// `at` plus `d`, rounded up to the next whole microsecond.
///
/// Ticks count microseconds and [`Tick::saturating_add`] drops anything finer,
/// which would put a deadline a hair early: the engine would come back while
/// the key still showed the old second, and then not again for a whole one.
/// Rounding up lands on the change or just after it.
fn after(at: Tick, d: Duration) -> Tick {
    let tick = at.saturating_add(d);
    if d.subsec_nanos().is_multiple_of(1000) {
        tick
    } else {
        Tick(tick.0.saturating_add(1))
    }
}

/// `left` as a share of `length`, 0 to 1. A zero-length timer is over the
/// moment it starts, so it has nothing left.
fn fraction_of(left: Duration, length: Duration) -> f32 {
    if length.is_zero() {
        return 0.0;
    }
    (left.as_secs_f64() / length.as_secs_f64()).clamp(0.0, 1.0) as f32
}

/// Whole seconds as `m:ss` below an hour and `h:mm:ss` from one.
fn clock_text(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Tick {
        Tick(n * 1000)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn timer(length: Duration) -> Countdown {
        Countdown::new(Some(length))
    }

    /// A timer of `length`, started at zero, as it reads `at` later.
    fn timer_shows(length: Duration, at: Duration) -> String {
        let mut t = timer(length);
        t.toggle(Tick::ZERO);
        t.display(Tick::ZERO.saturating_add(at)).text
    }

    /// A stopwatch started at zero, as it reads `at` later.
    fn stopwatch_shows(at: Duration) -> String {
        let mut w = Countdown::new(None);
        w.toggle(Tick::ZERO);
        w.display(Tick::ZERO.saturating_add(at)).text
    }

    #[test]
    fn a_new_timer_is_stopped_at_its_full_length() {
        let t = timer(secs(25 * 60));
        assert!(!t.running());
        assert!(!t.is_done());
        assert_eq!(t.end(), None);
        assert_eq!(t.next_change(Tick::ZERO), None);
        assert_eq!(
            t.display(ms(5000)),
            Display {
                text: "25:00".into(),
                fraction: Some(1.0),
                paused: false,
                done: false,
            }
        );
    }

    #[test]
    fn a_tap_starts_a_timer_and_the_next_pauses_it() {
        let mut t = timer(secs(60));
        t.toggle(ms(1000));
        assert!(t.running());
        assert_eq!(t.end(), Some(ms(61_000)));
        assert_eq!(t.remaining(ms(11_000)), Some(secs(50)));
        assert_eq!(t.elapsed(ms(11_000)), secs(10));
        assert_eq!(t.display(ms(11_000)).text, "0:50");
        assert_eq!(t.display(ms(16_000)).fraction, Some(0.75));

        t.toggle(ms(11_000));
        assert!(!t.running());
        assert_eq!(t.end(), None);
        assert_eq!(t.next_change(ms(11_000)), None);
        // Time passing while it is paused changes nothing.
        assert_eq!(t.remaining(ms(500_000)), Some(secs(50)));
        let shown = t.display(ms(500_000));
        assert_eq!(shown.text, "0:50");
        assert!(shown.paused);
        assert!(!shown.done);
    }

    #[test]
    fn a_paused_timer_resumes_where_it_stopped() {
        let mut t = timer(secs(60));
        t.toggle(ms(0));
        t.toggle(ms(10_000));
        t.toggle(ms(100_000));
        assert!(t.running());
        assert!(!t.display(ms(100_000)).paused);
        assert_eq!(t.end(), Some(ms(150_000)));
        assert_eq!(t.remaining(ms(110_000)), Some(secs(40)));
        assert_eq!(t.elapsed(ms(110_000)), secs(20));
    }

    #[test]
    fn a_timer_finishes_once_even_when_nobody_looks() {
        let mut t = timer(secs(60));
        t.toggle(Tick::ZERO);
        assert!(!t.finish_if_due(ms(59_999)));
        assert!(!t.is_done());

        // Nobody looks for an hour.
        assert!(t.finish_if_due(ms(3_600_000)));
        assert!(!t.finish_if_due(ms(3_600_001)));
        assert!(t.is_done());
        assert!(!t.running());
        assert_eq!(t.end(), None);
        assert_eq!(t.next_change(ms(3_600_000)), None);
        assert_eq!(t.remaining(ms(3_600_000)), Some(Duration::ZERO));
        assert_eq!(t.elapsed(ms(3_600_000)), secs(60));
        assert_eq!(
            t.display(ms(3_600_000)),
            Display {
                text: "0:00".into(),
                fraction: Some(0.0),
                paused: false,
                done: true,
            }
        );

        // It is done from the moment it ran out, not from when it was
        // noticed, so its flash was over long ago.
        assert!(!t.flashing(ms(3_600_000)));
        assert!(t.flashing(ms(69_999)));
        assert!(!t.flashing(ms(70_000)));
    }

    #[test]
    fn a_timer_finishes_exactly_at_its_end() {
        let mut t = timer(secs(60));
        t.toggle(ms(500));
        assert!(!t.finish_if_due(ms(60_499)));
        assert!(t.finish_if_due(ms(60_500)));
        assert!(t.flash_on(ms(60_500)));
    }

    #[test]
    fn a_tap_on_a_finished_timer_resets_it() {
        let mut t = timer(secs(60));
        t.toggle(Tick::ZERO);
        assert!(t.finish_if_due(ms(60_000)));

        t.toggle(ms(61_000));
        assert_eq!(t, timer(secs(60)));
        assert!(!t.flashing(ms(61_000)));
        assert_eq!(t.display(ms(61_000)).text, "1:00");

        // And the next tap runs it again.
        t.toggle(ms(62_000));
        assert_eq!(t.end(), Some(ms(122_000)));
    }

    #[test]
    fn a_running_countdown_refuses_a_reset() {
        let mut t = timer(secs(25 * 60));
        t.toggle(Tick::ZERO);
        assert_eq!(t.reset(ms(600_000)), Reset::Refused);
        assert!(t.running());
        assert_eq!(t.remaining(ms(600_000)), Some(secs(15 * 60)));
    }

    #[test]
    fn a_paused_countdown_resets() {
        let mut t = timer(secs(25 * 60));
        t.toggle(Tick::ZERO);
        t.toggle(ms(600_000));
        assert_eq!(t.reset(ms(700_000)), Reset::Done);
        assert_eq!(t, timer(secs(25 * 60)));
        let shown = t.display(ms(700_000));
        assert_eq!(shown.text, "25:00");
        assert!(!shown.paused);
    }

    #[test]
    fn a_finished_countdown_resets() {
        let mut t = timer(secs(60));
        t.toggle(Tick::ZERO);
        assert!(t.finish_if_due(ms(60_000)));
        assert_eq!(t.reset(ms(61_000)), Reset::Done);
        assert_eq!(t, timer(secs(60)));
    }

    #[test]
    fn a_countdown_that_has_run_out_resets_before_it_is_marked_done() {
        let mut t = timer(secs(60));
        t.toggle(Tick::ZERO);
        assert_eq!(t.reset(ms(60_000)), Reset::Done);
        assert_eq!(t, timer(secs(60)));
    }

    #[test]
    fn a_stopwatch_resets_while_running() {
        let mut w = Countdown::new(None);
        w.toggle(Tick::ZERO);
        assert_eq!(w.reset(ms(90_000)), Reset::Done);
        assert_eq!(w, Countdown::new(None));
        assert!(!w.running());
        assert_eq!(w.display(ms(95_000)).text, "0:00");
    }

    #[test]
    fn a_stopwatch_counts_up_across_pauses_and_never_finishes() {
        let mut w = Countdown::new(None);
        w.toggle(Tick::ZERO);
        assert_eq!(w.remaining(ms(5000)), None);
        assert_eq!(w.end(), None);
        assert_eq!(w.display(ms(5000)).fraction, None);

        w.toggle(ms(10_000));
        assert!(w.display(ms(15_000)).paused);
        w.toggle(ms(20_000));
        assert_eq!(w.elapsed(ms(25_000)), secs(15));
        assert_eq!(w.display(ms(81_000)).text, "1:11");

        assert!(!w.finish_if_due(Tick(u64::MAX)));
        assert!(!w.is_done());
        assert!(!w.flashing(ms(81_000)));
    }

    #[test]
    fn a_timer_shows_the_time_left_rounded_up() {
        assert_eq!(timer(Duration::ZERO).display(Tick::ZERO).text, "0:00");
        assert_eq!(
            timer(Duration::from_micros(1)).display(Tick::ZERO).text,
            "0:01"
        );
        assert_eq!(timer_shows(secs(60), secs(1)), "0:59");
        assert_eq!(timer_shows(secs(60), Duration::from_millis(500)), "1:00");
        assert_eq!(
            timer_shows(secs(60), Duration::from_micros(999_999)),
            "1:00"
        );
        assert_eq!(timer_shows(secs(60), secs(60)), "0:00");
        assert_eq!(timer_shows(secs(3599), Duration::ZERO), "59:59");
        assert_eq!(
            timer_shows(secs(3600), Duration::from_millis(500)),
            "1:00:00"
        );
        assert_eq!(timer_shows(secs(3600), secs(1)), "59:59");
        assert_eq!(timer_shows(secs(3601), Duration::ZERO), "1:00:01");
        assert_eq!(timer_shows(secs(36_000), Duration::ZERO), "10:00:00");
        assert_eq!(timer_shows(secs(25 * 3600), Duration::ZERO), "25:00:00");
    }

    #[test]
    fn a_stopwatch_shows_the_time_gone_rounded_down_and_stops_at_its_cap() {
        assert_eq!(stopwatch_shows(Duration::ZERO), "0:00");
        assert_eq!(stopwatch_shows(Duration::from_millis(999)), "0:00");
        assert_eq!(stopwatch_shows(secs(1)), "0:01");
        assert_eq!(stopwatch_shows(secs(59)), "0:59");
        assert_eq!(stopwatch_shows(secs(60)), "1:00");
        assert_eq!(stopwatch_shows(Duration::from_millis(3_599_999)), "59:59");
        assert_eq!(stopwatch_shows(secs(3600)), "1:00:00");
        assert_eq!(stopwatch_shows(secs(STOPWATCH_CAP)), "99:59:59");
        assert_eq!(stopwatch_shows(secs(100 * 3600)), "99:59:59");
        assert_eq!(stopwatch_shows(secs(10_000 * 3600)), "99:59:59");
    }

    #[test]
    fn next_change_lands_on_the_next_whole_second_of_the_display() {
        let mut t = timer(secs(10));
        t.toggle(Tick::ZERO);
        assert_eq!(t.next_change(Tick::ZERO), Some(ms(1000)));
        assert_eq!(t.next_change(ms(300)), Some(ms(1000)));
        assert_eq!(t.next_change(ms(1000)), Some(ms(2000)));
        // The last change is the end itself, when it reaches 0:00.
        assert_eq!(t.next_change(ms(9500)), t.end());
        assert_eq!(t.next_change(ms(10_000)), None);

        let mut w = Countdown::new(None);
        w.toggle(ms(250));
        assert_eq!(w.next_change(ms(250)), Some(ms(1250)));
        assert_eq!(w.next_change(ms(1300)), Some(ms(2250)));
        assert_eq!(
            w.next_change(ms(250 + (STOPWATCH_CAP - 1) * 1000 + 500)),
            Some(ms(250 + STOPWATCH_CAP * 1000))
        );
        assert_eq!(w.next_change(ms(250 + STOPWATCH_CAP * 1000)), None);

        // A pause part way through a second moves where the seconds fall.
        let mut paused = timer(secs(10));
        paused.toggle(Tick::ZERO);
        paused.toggle(ms(2300));
        paused.toggle(ms(5000));
        assert_eq!(paused.display(ms(5000)).text, "0:08");
        assert_eq!(paused.next_change(ms(5000)), Some(ms(5700)));

        // Whenever it is asked, the text holds up to that tick and changes on
        // it, so the engine never paints a second late or for nothing.
        for (c, started) in [(t, 0), (w, 250), (paused, 5000)] {
            for step in 0..400u64 {
                let now = ms(started + step * 37);
                let Some(next) = c.next_change(now) else {
                    continue;
                };
                let text = c.display(now).text;
                assert_eq!(c.display(Tick(next.0 - 1)).text, text, "at {now:?}");
                assert_ne!(c.display(next).text, text, "at {now:?}");
            }
        }
    }

    #[test]
    fn a_duration_finer_than_a_tick_rounds_its_deadlines_up() {
        let mut t = timer(Duration::from_nanos(1_500_000_500));
        t.toggle(Tick::ZERO);
        // Ends a whole microsecond late rather than a fraction early, so the
        // display has reached 0:00 by the time it is marked done.
        let end = t.end().unwrap();
        assert_eq!(end, Tick(1_500_001));
        assert_eq!(t.display(Tick(end.0 - 1)).text, "0:01");
        assert_eq!(t.display(end).text, "0:00");
        assert_eq!(t.next_change(Tick::ZERO), Some(Tick(500_001)));
        assert_eq!(t.display(Tick(500_000)).text, "0:02");
        assert_eq!(t.display(Tick(500_001)).text, "0:01");
    }

    #[test]
    fn the_flash_changes_at_most_twice_a_second_and_stops_after_ten_seconds() {
        let mut t = timer(secs(1));
        t.toggle(Tick::ZERO);
        assert!(t.finish_if_due(ms(1000)));
        let done_at = 1_000_000;

        // The engine paints whenever something wakes it -- a knob spinning,
        // a report arriving -- so look at uneven moments, a few microseconds
        // to a few milliseconds apart. Each change found between two looks is
        // narrowed down to the exact tick it happened on.
        let mut changes = Vec::new();
        let (mut now, mut stride) = (done_at, 1u64);
        let mut was = t.flash_on(Tick(now));
        assert!(was, "a finished timer starts bright");
        while now < done_at + 15_000_000 {
            let before = now;
            stride = (stride * 7919 + 13) % 9973;
            now += stride + 1;
            let on = t.flash_on(Tick(now));
            if on != was {
                let (mut lo, mut hi) = (before, now);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if t.flash_on(Tick(mid)) == was {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                changes.push(hi);
                was = on;
            }
        }

        for (i, &at) in changes.iter().enumerate() {
            let within = changes[i..]
                .iter()
                .take_while(|&&c| c < at + 1_000_000)
                .count();
            assert!(within <= 2, "{within} changes in the second from {at}");
        }
        // Bright and dark for half a second each, ending dark at 9.5 s.
        assert_eq!(changes.len(), 19);
        assert_eq!(changes.last(), Some(&(done_at + 9_500_000)));
        assert!(t.flashing(Tick(done_at + 9_999_999)));
        assert!(!t.flashing(Tick(done_at + 10_000_000)));
        assert!(!t.flash_on(Tick(done_at + 10_000_000)));
        // Resting at the alarm colour until a tap.
        assert!(t.is_done());
    }

    #[test]
    fn a_huge_duration_saturates_instead_of_panicking() {
        let late = Tick(u64::MAX - 5);
        let mut t = timer(Duration::MAX);
        assert!(!t.display(Tick::ZERO).text.is_empty());
        assert_eq!(t.display(Tick::ZERO).fraction, Some(1.0));
        t.toggle(late);
        assert_eq!(t.end(), Some(Tick(u64::MAX)));
        assert!(t.next_change(late).is_some());
        assert!(!t.display(Tick(u64::MAX)).text.is_empty());
        assert_eq!(t.reset(late), Reset::Refused);
        t.toggle(Tick(u64::MAX));
        t.toggle(Tick::ZERO);
        assert!(t.remaining(Tick(u64::MAX)).is_some());

        let mut w = Countdown::new(None);
        w.toggle(Tick::ZERO);
        assert_eq!(w.display(Tick(u64::MAX)).text, "99:59:59");
        assert_eq!(w.next_change(Tick(u64::MAX)), None);
        w.toggle(Tick(u64::MAX));
        w.toggle(Tick::ZERO);
        assert_eq!(
            w.elapsed(Tick(u64::MAX)),
            Duration::from_micros(u64::MAX) * 2
        );

        let mut w = Countdown::new(None);
        w.toggle(late);
        assert_eq!(w.next_change(late), Some(Tick(u64::MAX)));
    }
}
