//! Pure runtime machinery for the daemon.
//!
//! Nothing here reads the wall clock, touches the filesystem, opens a socket,
//! or knows what a `Galleon` is — a CI job enforces all four. What is left is
//! the part worth testing exhaustively: when things should happen, and in
//! what order.

pub mod clock;
pub mod scheduler;
pub mod wake;

pub use clock::{Clock, ManualClock, Tick};
pub use scheduler::{Scheduler, TimerId};
pub use wake::{wake_channel, WakeReceiver, Waker};
