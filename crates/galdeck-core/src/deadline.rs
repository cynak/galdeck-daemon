//! The next time anything wants to happen, shared across threads.
//!
//! The thread that owns the device cannot ask the scheduler directly — the
//! scheduler lives on the thread that owns all the mutable state, and that
//! thread must never block on the device. So the deadline is published into a
//! single atomic that the io thread reads to size its next poll.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::clock::Tick;

/// Sentinel for "nothing is scheduled".
const NONE: u64 = u64::MAX;

/// A one-writer, many-reader cell holding the next deadline.
#[derive(Debug)]
pub struct DeadlineCell {
    at: AtomicU64,
}

impl Default for DeadlineCell {
    fn default() -> Self {
        Self::new()
    }
}

impl DeadlineCell {
    pub fn new() -> Self {
        Self {
            at: AtomicU64::new(NONE),
        }
    }

    /// Publish the next deadline. The scheduler's owner is the only writer.
    ///
    /// This is a plain store, deliberately not a `fetch_min`. A monotonically
    /// non-increasing minimum would latch the earliest deadline ever seen and,
    /// once it fell into the past, clamp the reader to its shortest poll
    /// forever — burning a core to wait for something that already happened.
    pub fn publish(&self, at: Option<Tick>) {
        self.at.store(
            at.map_or(NONE, |tick| tick.0.min(NONE - 1)),
            Ordering::Release,
        );
    }

    /// The published deadline, if any.
    pub fn peek(&self) -> Option<Tick> {
        match self.at.load(Ordering::Acquire) {
            NONE => None,
            at => Some(Tick(at)),
        }
    }

    /// How long until the deadline, or `None` when nothing is scheduled.
    /// Returns `Some(ZERO)` for a deadline that has already passed.
    pub fn remaining(&self, now: Tick) -> Option<Duration> {
        self.peek()
            .map(|at| at.checked_duration_since(now).unwrap_or_default())
    }
}
