//! Gesture timing, named in milliseconds rather than waited out.

use std::time::Duration;

use galdeck_core::Tick;
use galdeck_daemon::input::{Bindings, Decision, Gesture, InputMachine};

const HOLD: Duration = Duration::from_millis(400);
const DOUBLE: Duration = Duration::from_millis(280);

fn at(ms: u64) -> Tick {
    Tick(ms * 1_000)
}

fn machine() -> InputMachine {
    InputMachine::with_windows(HOLD, DOUBLE)
}

const TAP_ONLY: Bindings = Bindings {
    has_tap: true,
    has_hold: false,
    has_double: false,
};
const TAP_AND_HOLD: Bindings = Bindings {
    has_tap: true,
    has_hold: true,
    has_double: false,
};
const TAP_AND_DOUBLE: Bindings = Bindings {
    has_tap: true,
    has_hold: false,
    has_double: true,
};
const ALL_THREE: Bindings = Bindings {
    has_tap: true,
    has_hold: true,
    has_double: true,
};

#[test]
fn a_plain_key_fires_the_instant_it_goes_down() {
    // The latency rule. Waiting to see whether a second tap arrives would add
    // a quarter-second to every keypress on the deck to serve a binding the
    // user did not ask for.
    let mut m = machine();
    assert_eq!(m.down(0, at(0), TAP_ONLY), Decision::Fire(Gesture::Tap));
    assert_eq!(m.up(0, at(50), TAP_ONLY), Decision::Idle);
}

#[test]
fn a_plain_key_with_no_binding_does_nothing() {
    let mut m = machine();
    assert_eq!(m.down(0, at(0), Bindings::default()), Decision::Idle);
}

#[test]
fn a_key_with_a_hold_waits_to_see_before_it_taps() {
    let mut m = machine();
    assert_eq!(m.down(0, at(0), TAP_AND_HOLD), Decision::Wait(at(400)));
    // Released early: a tap, on release rather than on press.
    assert_eq!(m.up(0, at(100), TAP_AND_HOLD), Decision::Fire(Gesture::Tap));
}

#[test]
fn holding_past_the_window_fires_the_hold() {
    let mut m = machine();
    m.down(0, at(0), TAP_AND_HOLD);
    assert_eq!(
        m.timeout(0, at(400), TAP_AND_HOLD),
        Decision::Fire(Gesture::Hold)
    );
}

#[test]
fn releasing_after_a_hold_does_not_also_tap() {
    // Otherwise every hold runs two actions, which is the sort of thing that
    // is only noticed once it has done something irreversible.
    let mut m = machine();
    m.down(0, at(0), TAP_AND_HOLD);
    assert_eq!(
        m.timeout(0, at(400), TAP_AND_HOLD),
        Decision::Fire(Gesture::Hold)
    );
    assert_eq!(m.up(0, at(900), TAP_AND_HOLD), Decision::Idle);
}

#[test]
fn a_hold_fires_once_however_long_it_is_held() {
    let mut m = machine();
    m.down(0, at(0), TAP_AND_HOLD);
    assert_eq!(
        m.timeout(0, at(400), TAP_AND_HOLD),
        Decision::Fire(Gesture::Hold)
    );
    assert_eq!(m.timeout(0, at(1200), TAP_AND_HOLD), Decision::Idle);
    assert_eq!(m.timeout(0, at(5000), TAP_AND_HOLD), Decision::Idle);
}

#[test]
fn a_timeout_arriving_early_does_not_fire_a_hold() {
    // The scheduler can fire a hair early; a hold is defined by the elapsed
    // time, not by the timer having gone off.
    let mut m = machine();
    m.down(0, at(0), TAP_AND_HOLD);
    assert_eq!(m.timeout(0, at(399), TAP_AND_HOLD), Decision::Idle);
    assert_eq!(
        m.timeout(0, at(400), TAP_AND_HOLD),
        Decision::Fire(Gesture::Hold)
    );
}

#[test]
fn a_single_tap_on_a_double_bound_key_waits_out_the_window() {
    let mut m = machine();
    assert_eq!(m.down(0, at(0), TAP_AND_DOUBLE), Decision::Idle);
    // The tap is held back until the window closes.
    assert_eq!(m.up(0, at(60), TAP_AND_DOUBLE), Decision::Wait(at(340)));
    assert_eq!(m.timeout(0, at(339), TAP_AND_DOUBLE), Decision::Idle);
    assert_eq!(
        m.timeout(0, at(340), TAP_AND_DOUBLE),
        Decision::Fire(Gesture::Tap)
    );
}

#[test]
fn two_taps_inside_the_window_are_a_double() {
    let mut m = machine();
    m.down(0, at(0), TAP_AND_DOUBLE);
    m.up(0, at(60), TAP_AND_DOUBLE);
    assert_eq!(
        m.down(0, at(200), TAP_AND_DOUBLE),
        Decision::Fire(Gesture::Double)
    );
    // And the held-back first tap is gone, not merely deferred.
    assert_eq!(m.up(0, at(240), TAP_AND_DOUBLE), Decision::Wait(at(520)));
    assert_eq!(
        m.timeout(0, at(520), TAP_AND_DOUBLE),
        Decision::Fire(Gesture::Tap)
    );
}

#[test]
fn two_taps_outside_the_window_are_two_taps() {
    let mut m = machine();
    m.down(0, at(0), TAP_AND_DOUBLE);
    m.up(0, at(60), TAP_AND_DOUBLE);
    assert_eq!(
        m.timeout(0, at(340), TAP_AND_DOUBLE),
        Decision::Fire(Gesture::Tap)
    );
    // Well past the window: an ordinary first press again.
    assert_eq!(m.down(0, at(900), TAP_AND_DOUBLE), Decision::Idle);
}

#[test]
fn a_hold_beats_a_double_when_both_are_bound() {
    // Holding is unambiguous the moment the window passes, so it does not wait
    // to see whether a second tap follows.
    let mut m = machine();
    assert_eq!(m.down(0, at(0), ALL_THREE), Decision::Wait(at(400)));
    assert_eq!(
        m.timeout(0, at(400), ALL_THREE),
        Decision::Fire(Gesture::Hold)
    );
    assert_eq!(m.up(0, at(500), ALL_THREE), Decision::Idle);
}

#[test]
fn keys_do_not_interfere_with_each_other() {
    let mut m = machine();
    m.down(0, at(0), TAP_AND_DOUBLE);
    m.up(0, at(50), TAP_AND_DOUBLE);
    // A different key going down is not the second tap of key 0.
    assert_eq!(m.down(1, at(100), TAP_AND_DOUBLE), Decision::Idle);
    assert_eq!(
        m.timeout(0, at(330), TAP_AND_DOUBLE),
        Decision::Fire(Gesture::Tap)
    );
}

#[test]
fn forgetting_drops_a_tap_held_back_on_a_page_that_has_gone() {
    // Otherwise a tap withheld on one page fires against whichever key takes
    // its position on the next one.
    let mut m = machine();
    m.down(0, at(0), TAP_AND_DOUBLE);
    m.up(0, at(50), TAP_AND_DOUBLE);
    m.forget();
    assert_eq!(m.timeout(0, at(330), TAP_AND_DOUBLE), Decision::Idle);
}

#[test]
fn a_release_with_no_press_is_ignored() {
    // The device reports state snapshots, so a release can arrive for a press
    // that happened before the daemon was watching.
    let mut m = machine();
    assert_eq!(m.up(0, at(100), TAP_AND_HOLD), Decision::Idle);
    assert_eq!(m.timeout(0, at(500), TAP_AND_HOLD), Decision::Idle);
}

#[test]
fn a_key_bound_only_to_hold_does_nothing_on_a_short_press() {
    let hold_only = Bindings {
        has_tap: false,
        has_hold: true,
        has_double: false,
    };
    let mut m = machine();
    assert_eq!(m.down(0, at(0), hold_only), Decision::Wait(at(400)));
    assert_eq!(m.up(0, at(100), hold_only), Decision::Idle);
}
