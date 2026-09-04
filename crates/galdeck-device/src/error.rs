//! Telling "the keyboard is gone" apart from "you asked for key 99".
//!
//! `galdeck::Error` has no unplug variant: a hot-unplug and a bad key index
//! both surface as one of five variants, and the daemon's reconnect loop
//! cannot tell them apart. That is survivable while the only reaction is a
//! log line, and not survivable once a pump reconnects on error — a
//! bad-argument bug would spin the reconnect loop forever, and each attempt
//! blocks 1.2 seconds.

use std::fmt;

/// What the caller should do about a failed device operation.
#[derive(Debug)]
pub enum DeckError {
    /// The device is gone. Drop the handle and reconnect.
    Disconnected(galdeck::Error),
    /// A transport hiccup. Worth retrying without tearing the handle down.
    Transient(galdeck::Error),
    /// The request itself was impossible. Reconnecting will not help, and
    /// retrying will not either — this is a bug in the caller.
    Invalid(galdeck::Error),
}

impl DeckError {
    /// Classify an error from an already-open device.
    ///
    /// `hidapi` reports a vanished device as a generic write/read failure, so
    /// any `Hid` error after a successful open is treated as an unplug. That
    /// is the safe direction: reconnecting after a transient glitch costs one
    /// re-open, while failing to reconnect after an unplug leaves a dead
    /// daemon.
    pub fn classify(source: galdeck::Error) -> Self {
        match source {
            galdeck::Error::DeviceNotFound => DeckError::Disconnected(source),
            galdeck::Error::Hid(_) => DeckError::Disconnected(source),
            // A garbled input report is worth ignoring rather than
            // reconnecting over; the framework already skips undecodable ones.
            galdeck::Error::MalformedReport(_) => DeckError::Transient(source),
            galdeck::Error::InvalidArgument(_) => DeckError::Invalid(source),
            galdeck::Error::Image(_) => DeckError::Invalid(source),
        }
    }

    /// Whether the handle should be torn down and reopened.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, DeckError::Disconnected(_))
    }

    pub fn source_error(&self) -> &galdeck::Error {
        match self {
            DeckError::Disconnected(e) | DeckError::Transient(e) | DeckError::Invalid(e) => e,
        }
    }
}

impl fmt::Display for DeckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeckError::Disconnected(e) => write!(f, "device disconnected: {e}"),
            DeckError::Transient(e) => write!(f, "device hiccup: {e}"),
            DeckError::Invalid(e) => write!(f, "invalid device request: {e}"),
        }
    }
}

impl std::error::Error for DeckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source_error())
    }
}

impl From<galdeck::Error> for DeckError {
    fn from(source: galdeck::Error) -> Self {
        Self::classify(source)
    }
}

pub type DeckResult<T> = Result<T, DeckError>;
