//! The real clock.
//!
//! The binary is the only layer allowed to read the wall clock; a CI job
//! enforces that everything below it takes time as a value instead. That is
//! what lets a sixty-second keepalive soak run in microseconds.

use std::time::Instant;

use galdeck_core::{Clock, Tick};

pub struct SystemClock {
    base: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Tick {
        // Microseconds since the engine started. u64 of those is 584,000
        // years, so the cast cannot realistically saturate.
        Tick(self.base.elapsed().as_micros() as u64)
    }
}
