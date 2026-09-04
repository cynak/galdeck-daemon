//! The galdeck configuration model.
//!
//! Everything here is pure: it parses, validates and describes configuration,
//! and never touches the device, the wall clock, a socket or a subprocess. A
//! CI job enforces that, which is what lets a user interface validate an edit
//! the user has not saved yet without any of the daemon's machinery.

pub mod diag;
pub mod v1;

pub use diag::{Diagnostic, Diagnostics, LineIndex, Loc, Severity};
pub use v1::{default_config_path, Config, EncoderConfig, KeyConfig, LoadError, Page, ParseError};
