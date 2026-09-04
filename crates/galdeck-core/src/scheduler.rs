//! A deadline heap.
//!
//! Today the daemon's entire notion of "something is moving" is
//! `RingFeedback::is_active()` — a boolean that shortens the poll timeout from
//! 200 ms to 40 ms. That cannot express a clock widget refreshing every
//! second, a sprite advancing at 30 Hz, and a hold-repeat firing every 80 ms
//! all at once, each with its own next deadline.
//!
//! This replaces it: everything that wants to happen later registers a
//! deadline, and the loop sleeps until the nearest one. Idle costs nothing,
//! because an empty heap means "no deadline" and the loop simply waits for
//! input.
//!
//! Generic over the payload so this crate stays free of every id type the
//! later phases introduce.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Duration;

use crate::clock::Tick;

/// How far behind a repeating timer may fall before it gives up on the
/// backlog and resynchronises.
///
/// A knob spun during a stall must not replay its animation seconds later.
/// The existing `RotationRunner` makes the same choice by dropping detents
/// when its queue is full rather than paying them off after the user stopped
/// turning.
const MAX_CATCH_UP_PERIODS: u64 = 2;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct TimerId(u64);

#[derive(Debug)]
struct Live<K> {
    kind: K,
    /// `Some` for a repeating timer.
    period: Option<Duration>,
}

/// A min-heap of deadlines, keyed by an opaque payload.
#[derive(Debug)]
pub struct Scheduler<K> {
    /// `(when, seq, id)`. `seq` breaks ties so equal deadlines fire in the
    /// order they were registered, which keeps tests deterministic.
    heap: BinaryHeap<Reverse<(Tick, u64, TimerId)>>,
    live: HashMap<TimerId, Live<K>>,
    next_id: u64,
    next_seq: u64,
}

impl<K> Default for Scheduler<K> {
    fn default() -> Self {
        Self {
            heap: BinaryHeap::new(),
            live: HashMap::new(),
            next_id: 0,
            next_seq: 0,
        }
    }
}

impl<K: Copy + Eq + Hash + Debug> Scheduler<K> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Number of live timers. Cancelled entries left in the heap are not
    /// counted; they are dropped when they surface.
    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn at(&mut self, when: Tick, kind: K) -> TimerId {
        self.insert(when, kind, None)
    }

    pub fn after(&mut self, now: Tick, delay: Duration, kind: K) -> TimerId {
        self.insert(now.saturating_add(delay), kind, None)
    }

    /// A timer that re-arms itself after each firing.
    ///
    /// Re-arming is measured from the deadline that just fired, not from the
    /// moment it was handled, so a periodic timer does not drift by the
    /// handler's cost on every tick.
    pub fn every(&mut self, first: Tick, period: Duration, kind: K) -> TimerId {
        assert!(
            !period.is_zero(),
            "a repeating timer needs a non-zero period"
        );
        self.insert(first, kind, Some(period))
    }

    fn insert(&mut self, when: Tick, kind: K, period: Option<Duration>) -> TimerId {
        let id = TimerId(self.next_id);
        self.next_id += 1;
        let seq = self.next_seq;
        self.next_seq += 1;
        self.live.insert(id, Live { kind, period });
        self.heap.push(Reverse((when, seq, id)));
        id
    }

    /// Cancelling only forgets the payload; the heap entry is dropped when it
    /// surfaces. That keeps cancel O(1) at the cost of a little dead weight,
    /// which is the right trade when re-arming is common.
    pub fn cancel(&mut self, id: TimerId) {
        self.live.remove(&id);
    }

    /// Cancel every timer carrying this payload.
    ///
    /// This is what makes re-arming idempotent: a widget whose interval
    /// changed cancels its kind and schedules afresh, without tracking ids.
    pub fn cancel_kind(&mut self, kind: K) {
        self.live.retain(|_, live| live.kind != kind);
    }

    pub fn contains_kind(&self, kind: K) -> bool {
        self.live.values().any(|live| live.kind == kind)
    }

    /// Everything due at or before `now`, in deadline order.
    ///
    /// Repeating timers are re-armed before this returns, so a handler may
    /// cancel its own kind without racing the re-arm.
    pub fn due(&mut self, now: Tick) -> Vec<(TimerId, K)> {
        let mut fired = Vec::new();
        while let Some(&Reverse((when, _, id))) = self.heap.peek() {
            if when > now {
                break;
            }
            self.heap.pop();
            let Some(live) = self.live.get(&id) else {
                // Cancelled while queued; drop it now.
                continue;
            };
            let kind = live.kind;
            match live.period {
                None => {
                    self.live.remove(&id);
                }
                Some(period) => {
                    let next = Self::rearm(when, now, period);
                    let seq = self.next_seq;
                    self.next_seq += 1;
                    self.heap.push(Reverse((next, seq, id)));
                }
            }
            fired.push((id, kind));
        }
        fired
    }

    /// Where a repeating timer's next deadline lands.
    fn rearm(when: Tick, now: Tick, period: Duration) -> Tick {
        let behind = now.duration_since(when).as_micros();
        let period_us = period.as_micros().max(1);
        if behind / period_us >= u128::from(MAX_CATCH_UP_PERIODS) {
            // Too far behind to be worth catching up: resynchronise rather
            // than firing a burst for time that has already passed.
            now.saturating_add(period)
        } else {
            when.saturating_add(period)
        }
    }

    /// How long until the next deadline, or `None` when nothing is scheduled.
    ///
    /// Returns `Some(ZERO)` when something is already due. Cancelled entries
    /// are skipped, which is where the heap's dead weight is reclaimed.
    pub fn next_deadline(&mut self, now: Tick) -> Option<Duration> {
        while let Some(&Reverse((when, _, id))) = self.heap.peek() {
            if self.live.contains_key(&id) {
                return Some(when.checked_duration_since(now).unwrap_or_default());
            }
            self.heap.pop();
        }
        None
    }
}
