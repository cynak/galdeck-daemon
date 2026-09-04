//! The galdeck daemon.
//!
//! Exposed as a library as well as a binary so the engine loop can be driven
//! from integration tests against a virtual deck, with no hardware attached.

pub mod clock;
pub mod engine;
pub mod http;
pub mod io;
pub mod ipc_server;
pub mod preview;
pub mod render;
pub mod ring;
pub mod widgets;
