//! A one-slot wake signal.
//!
//! The core loop sleeps until its next deadline. Anything that wants it to
//! look sooner — an input event, a control request, a finished render — sends
//! on its own typed channel and then pings the waker.
//!
//! The slot holds one token because one pending wake-up is exactly as
//! informative as a hundred: the loop drains every inbox when it wakes. That
//! is what makes `notify()` non-blocking and allocation-free, and why no
//! producer can ever be stalled by a busy core.

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Waker(SyncSender<()>);

#[derive(Debug)]
pub struct WakeReceiver(Receiver<()>);

pub fn wake_channel() -> (Waker, WakeReceiver) {
    let (tx, rx) = sync_channel(1);
    (Waker(tx), WakeReceiver(rx))
}

impl Waker {
    /// Ask the core loop to wake. Never blocks.
    ///
    /// A full slot means a wake-up is already pending, and a disconnected
    /// receiver means the loop has stopped; neither is an error a producer
    /// can do anything about.
    pub fn notify(&self) {
        match self.0.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {}
        }
    }
}

impl WakeReceiver {
    /// Wait up to `timeout` for a wake-up.
    ///
    /// Returns `false` only when every waker has been dropped, which is the
    /// loop's signal to exit.
    pub fn wait(&self, timeout: Duration) -> bool {
        match self.0.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    /// Consume a pending wake-up if there is one.
    pub fn drain(&self) {
        while self.0.try_recv().is_ok() {}
    }
}
