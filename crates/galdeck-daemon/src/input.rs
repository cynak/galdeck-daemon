//! Turning presses into gestures.
//!
//! The framework reports edges and nothing else: key 3 went down, key 3 came
//! up, with no timestamps and no notion of how long or how often. Everything
//! above that — a hold, a double tap, and the decision of *when* a plain tap
//! has definitely happened — is this module's job.
//!
//! It is a pure state machine over an injected clock. That is not fastidious:
//! the windows here are hundreds of milliseconds, and a test suite that waited
//! them out in real time would take minutes and still be flaky.
//!
//! # The latency rule
//!
//! A key with only `exec` fires the instant it goes down, exactly as it always
//! has. Waiting to see whether a second tap arrives would add a quarter-second
//! to every keypress on the deck to serve a binding the user did not ask for.
//! Only a key that actually declares `hold` or `double` pays anything, and it
//! pays only what disambiguating it requires.

use std::collections::BTreeMap;
use std::time::Duration;

use galdeck_core::Tick;

/// How long a key must be held before it counts as a hold.
///
/// Chosen against the io thread's 5 ms poll while anything is happening, so
/// the measurement is accurate to well within a frame. Shorter starts firing
/// holds at people who are merely deliberate.
pub const HOLD_MS: u64 = 400;
/// How long to wait for a second tap.
///
/// This is the entire cost of binding `double` to a key, so it is as short as
/// it can be while still being reachable by a human hand.
pub const DOUBLE_TAP_MS: u64 = 280;

/// What a key was asked to do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bindings {
    pub has_tap: bool,
    pub has_hold: bool,
    pub has_double: bool,
}

impl Bindings {
    /// Whether this key can be answered the moment it goes down.
    fn is_immediate(&self) -> bool {
        !self.has_hold && !self.has_double
    }
}

/// Something the user did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    Tap,
    Hold,
    Double,
}

/// What the caller should do about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run this gesture's action.
    Fire(Gesture),
    /// Nothing yet. Call [`InputMachine::timeout`] for this key at this time.
    Wait(Tick),
    /// Nothing to do, and no timer needed.
    Idle,
}

#[derive(Clone, Copy, Debug, Default)]
struct KeyState {
    /// When it went down, while it is down.
    down_at: Option<Tick>,
    /// Set once a hold has fired, so releasing does not also fire a tap.
    hold_fired: bool,
    /// A completed tap waiting to see whether a second one follows.
    pending_tap_until: Option<Tick>,
}

/// Per-key gesture state.
#[derive(Debug, Default)]
pub struct InputMachine {
    keys: BTreeMap<u8, KeyState>,
    hold: Duration,
    double_tap: Duration,
}

impl InputMachine {
    pub fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
            hold: Duration::from_millis(HOLD_MS),
            double_tap: Duration::from_millis(DOUBLE_TAP_MS),
        }
    }

    /// With windows of your choosing, for tests.
    pub fn with_windows(hold: Duration, double_tap: Duration) -> Self {
        Self {
            keys: BTreeMap::new(),
            hold,
            double_tap,
        }
    }

    /// A key went down.
    pub fn down(&mut self, key: u8, now: Tick, bindings: Bindings) -> Decision {
        // The common case: nothing to disambiguate, so answer immediately.
        if bindings.is_immediate() {
            return if bindings.has_tap {
                Decision::Fire(Gesture::Tap)
            } else {
                Decision::Idle
            };
        }

        let state = self.keys.entry(key).or_default();
        let second_tap = state.pending_tap_until.is_some_and(|until| now < until);
        state.down_at = Some(now);
        state.hold_fired = false;

        if second_tap && bindings.has_double {
            state.pending_tap_until = None;
            return Decision::Fire(Gesture::Double);
        }
        state.pending_tap_until = None;

        // Wait: either to see whether it is held, or to see whether a second
        // tap follows once it is released.
        if bindings.has_hold {
            Decision::Wait(now.saturating_add(self.hold))
        } else {
            Decision::Idle
        }
    }

    /// A key came up.
    pub fn up(&mut self, key: u8, now: Tick, bindings: Bindings) -> Decision {
        if bindings.is_immediate() {
            // Already answered on the way down.
            return Decision::Idle;
        }
        let double_tap = self.double_tap;
        let Some(state) = self.keys.get_mut(&key) else {
            return Decision::Idle;
        };
        state.down_at = None;

        if state.hold_fired {
            // The hold already happened; releasing is not also a tap.
            state.hold_fired = false;
            return Decision::Idle;
        }

        if bindings.has_double {
            // Hold the tap back until the double-tap window closes.
            let until = now.saturating_add(double_tap);
            state.pending_tap_until = Some(until);
            return Decision::Wait(until);
        }

        if bindings.has_tap {
            Decision::Fire(Gesture::Tap)
        } else {
            Decision::Idle
        }
    }

    /// A deadline this machine asked for has arrived.
    pub fn timeout(&mut self, key: u8, now: Tick, bindings: Bindings) -> Decision {
        let hold = self.hold;
        let Some(state) = self.keys.get_mut(&key) else {
            return Decision::Idle;
        };

        // Still down, and long enough: that is a hold.
        if let Some(down_at) = state.down_at {
            if bindings.has_hold && !state.hold_fired && now.duration_since(down_at) >= hold {
                state.hold_fired = true;
                return Decision::Fire(Gesture::Hold);
            }
            return Decision::Idle;
        }

        // Released, and the double-tap window has closed without a second
        // press: it was a plain tap after all.
        if let Some(until) = state.pending_tap_until {
            if now >= until {
                state.pending_tap_until = None;
                return if bindings.has_tap {
                    Decision::Fire(Gesture::Tap)
                } else {
                    Decision::Idle
                };
            }
        }
        Decision::Idle
    }

    /// Forget everything, for a page or profile switch.
    ///
    /// A tap held back on the page being left must not fire against whatever
    /// key takes its place.
    pub fn forget(&mut self) {
        self.keys.clear();
    }
}
