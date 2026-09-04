//! Time, as a value the tests can name.
//!
//! Nothing in this crate reads the wall clock. `ring.rs` already showed why
//! that pays: its eight tests inject an `Instant` and are the only fully
//! deterministic tests in the daemon. Generalising that discipline is what
//! lets a sixty-second animation soak run in microseconds on CI.
//!
//! The real implementation lives in the daemon binary, which is the only
//! layer allowed to call `Instant::now()`; a CI job enforces that.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Microseconds since the engine started.
///
/// A newtype rather than `Instant` so tests can name exact times, logs can
/// print them, and the debug panel can serialize them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Default)]
pub struct Tick(pub u64);

impl Tick {
    pub const ZERO: Tick = Tick(0);

    /// Saturating, because a deadline far enough in the future is simply
    /// "never" rather than a panic or a wrap into the past.
    pub fn saturating_add(self, d: Duration) -> Tick {
        Tick(
            self.0
                .saturating_add(d.as_micros().min(u128::from(u64::MAX)) as u64),
        )
    }

    /// How long since `earlier`. Saturates at zero rather than going negative,
    /// so a timer that fires a hair early is harmless.
    pub fn duration_since(self, earlier: Tick) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0))
    }

    /// `None` when `self` is not after `earlier`.
    pub fn checked_duration_since(self, earlier: Tick) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_micros)
    }
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Tick;
}

/// A clock the tests move by hand.
///
/// `Sync` and cheap to clone-by-reference so a test can hold it while the
/// code under test holds an `Arc` of the same one.
#[derive(Debug, Default)]
pub struct ManualClock {
    us: AtomicU64,
}

impl ManualClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn starting_at(tick: Tick) -> Self {
        Self {
            us: AtomicU64::new(tick.0),
        }
    }

    /// Move time forward and return the new instant.
    pub fn advance(&self, d: Duration) -> Tick {
        let by = d.as_micros().min(u128::from(u64::MAX)) as u64;
        Tick(self.us.fetch_add(by, Ordering::SeqCst) + by)
    }

    pub fn set(&self, tick: Tick) {
        self.us.store(tick.0, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Tick {
        Tick(self.us.load(Ordering::SeqCst))
    }
}
