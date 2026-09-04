//! The scheduler decides when everything in the daemon happens, so it is
//! tested the way `ring.rs` is: pure, with time named explicitly.

use std::time::Duration;

use galdeck_core::{Clock, ManualClock, Scheduler, Tick};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Animation(u8),
    Widget(u8),
    Keepalive,
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn fires_in_deadline_order_not_registration_order() {
    let mut s = Scheduler::new();
    s.at(Tick(300), Kind::Widget(1));
    s.at(Tick(100), Kind::Animation(1));
    s.at(Tick(200), Kind::Animation(2));

    let fired: Vec<_> = s.due(Tick(1000)).into_iter().map(|(_, k)| k).collect();
    assert_eq!(
        fired,
        vec![Kind::Animation(1), Kind::Animation(2), Kind::Widget(1)]
    );
}

#[test]
fn equal_deadlines_fire_in_registration_order() {
    let mut s = Scheduler::new();
    s.at(Tick(100), Kind::Animation(1));
    s.at(Tick(100), Kind::Animation(2));
    s.at(Tick(100), Kind::Animation(3));

    let fired: Vec<_> = s.due(Tick(100)).into_iter().map(|(_, k)| k).collect();
    assert_eq!(
        fired,
        vec![Kind::Animation(1), Kind::Animation(2), Kind::Animation(3)]
    );
}

#[test]
fn nothing_fires_before_its_deadline() {
    let mut s = Scheduler::new();
    s.at(Tick(100), Kind::Animation(1));
    assert!(s.due(Tick(99)).is_empty());
    assert_eq!(s.due(Tick(100)).len(), 1);
    // And a one-shot does not fire twice.
    assert!(s.due(Tick(1000)).is_empty());
}

#[test]
fn an_empty_scheduler_has_no_deadline() {
    // This is the idle case: no deadline means the loop waits for input
    // instead of spinning, which is what keeps an idle daemon at zero CPU.
    let mut s: Scheduler<Kind> = Scheduler::new();
    assert_eq!(s.next_deadline(Tick(0)), None);
    assert!(s.is_empty());
}

#[test]
fn next_deadline_is_the_nearest_and_zero_when_overdue() {
    let mut s = Scheduler::new();
    s.at(Tick(5_000), Kind::Widget(1));
    s.at(Tick(1_000), Kind::Animation(1));
    assert_eq!(s.next_deadline(Tick(0)), Some(Duration::from_micros(1_000)));
    // Already due reads as zero, not as "nothing scheduled".
    assert_eq!(s.next_deadline(Tick(2_000)), Some(Duration::ZERO));
}

#[test]
fn cancel_before_firing_drops_the_timer() {
    let mut s = Scheduler::new();
    let id = s.at(Tick(100), Kind::Animation(1));
    s.at(Tick(200), Kind::Widget(1));
    s.cancel(id);

    let fired: Vec<_> = s.due(Tick(1000)).into_iter().map(|(_, k)| k).collect();
    assert_eq!(fired, vec![Kind::Widget(1)]);
}

#[test]
fn a_cancelled_timer_does_not_hold_the_next_deadline() {
    // Cancel is O(1) and leaves the heap entry behind, so next_deadline has
    // to skip it -- otherwise the loop would wake for a timer that no longer
    // exists.
    let mut s = Scheduler::new();
    let id = s.at(Tick(100), Kind::Animation(1));
    s.at(Tick(900), Kind::Widget(1));
    s.cancel(id);
    assert_eq!(s.next_deadline(Tick(0)), Some(Duration::from_micros(900)));
}

#[test]
fn cancel_kind_makes_rearming_idempotent() {
    let mut s = Scheduler::new();
    s.every(Tick(100), ms(1), Kind::Widget(7));
    s.every(Tick(150), ms(1), Kind::Widget(7));
    s.at(Tick(200), Kind::Animation(1));
    assert_eq!(s.len(), 3);

    // A widget whose interval changed cancels its kind and schedules afresh
    // without having to track ids.
    s.cancel_kind(Kind::Widget(7));
    assert!(!s.contains_kind(Kind::Widget(7)));
    let fired: Vec<_> = s.due(Tick(10_000)).into_iter().map(|(_, k)| k).collect();
    assert_eq!(fired, vec![Kind::Animation(1)]);
}

#[test]
fn a_repeating_timer_does_not_drift() {
    // Re-arming measures from the deadline that fired, not from the moment it
    // was handled, so a 100 ms timer handled 30 ms late still fires next at
    // 200 ms rather than 230 ms.
    let mut s = Scheduler::new();
    s.every(Tick(100_000), ms(100), Kind::Keepalive);

    let handled_late = Tick(130_000);
    assert_eq!(s.due(handled_late).len(), 1);
    assert_eq!(
        s.next_deadline(Tick(130_000)),
        Some(Duration::from_micros(70_000)),
        "next fire should be at 200 ms, i.e. 70 ms after 130 ms"
    );
}

#[test]
fn a_repeating_timer_resynchronises_after_a_long_stall() {
    // Opening the device blocks 1.2 s and a keepalive after a gap blocks
    // another second. A 30 Hz animation must not then fire ~36 queued frames
    // for time that has already passed.
    let mut s = Scheduler::new();
    s.every(Tick(0), ms(33), Kind::Animation(1));

    let after_stall = Tick(1_200_000);
    let fired = s.due(after_stall);
    assert_eq!(fired.len(), 1, "one catch-up frame, not the whole backlog");

    // And the next deadline is one period from now, not buried in the past.
    assert_eq!(
        s.next_deadline(after_stall),
        Some(Duration::from_micros(33_000))
    );
}

#[test]
fn a_slightly_late_repeating_timer_still_catches_up_once() {
    // One missed period is worth catching up; that is the difference between
    // a hiccup and a stall.
    let mut s = Scheduler::new();
    s.every(Tick(0), ms(10), Kind::Animation(1));
    s.due(Tick(0));
    // 15 ms in: the 10 ms deadline passed, so it is due immediately.
    assert_eq!(s.next_deadline(Tick(15_000)), Some(Duration::ZERO));
}

#[test]
fn many_timers_interleave_correctly() {
    let mut s = Scheduler::new();
    s.every(Tick(0), ms(500), Kind::Keepalive); // the module's heartbeat
    s.every(Tick(0), ms(33), Kind::Animation(1)); // a 30 Hz sprite
    s.every(Tick(0), ms(1000), Kind::Widget(1)); // a clock

    // Everything that is due at t=0 fires once.
    assert_eq!(s.due(Tick(0)).len(), 3);

    // Over the next second the sprite dominates, the keepalive fires twice
    // and the clock once.
    let mut counts = (0, 0, 0);
    let mut now = Tick(0);
    for _ in 0..1000 {
        now = now.saturating_add(ms(1));
        for (_, kind) in s.due(now) {
            match kind {
                Kind::Animation(_) => counts.0 += 1,
                Kind::Keepalive => counts.1 += 1,
                Kind::Widget(_) => counts.2 += 1,
            }
        }
    }
    assert_eq!(counts.0, 30, "30 Hz sprite over one second");
    assert_eq!(counts.1, 2, "keepalive at 500 ms and 1000 ms");
    assert_eq!(counts.2, 1, "clock at 1000 ms");
}

#[test]
fn the_manual_clock_moves_only_when_told() {
    let clock = ManualClock::new();
    assert_eq!(clock.now(), Tick::ZERO);
    clock.advance(ms(250));
    assert_eq!(clock.now(), Tick(250_000));
    clock.advance(ms(250));
    assert_eq!(clock.now(), Tick(500_000));
    assert_eq!(clock.now().duration_since(Tick::ZERO), ms(500));
}
