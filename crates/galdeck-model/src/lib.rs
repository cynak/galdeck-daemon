//! The galdeck configuration model.
//!
//! Everything here is pure: it parses, validates and describes configuration,
//! and never touches the device, the wall clock, a socket or a subprocess. A
//! CI job enforces that, which is what lets a user interface validate an edit
//! the user has not saved yet without any of the daemon's machinery.

pub mod color;
pub mod diag;
pub mod theme;
pub mod v1;
pub mod v2;
pub mod workspace;

pub use color::{ColorRef, Palette, ResolvedPalette};
pub use diag::{Diagnostic, Diagnostics, LineIndex, Loc, Severity};
pub use theme::{ResolvedStyle, StyleLayer, StyleSource, Theme};
pub use v1::{default_config_path, Config, EncoderConfig, KeyConfig, LoadError, Page, ParseError};
pub use v2::{Global, Workspace, CURRENT_VERSION};
pub use workspace::default_config_dir;

/// Levenshtein distance, iterative with one row of state.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b_chars.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            let insert_delete = (row[j] + 1).min(row[j + 1] + 1);
            let substitute = previous + cost;
            previous = row[j + 1];
            row[j + 1] = insert_delete.min(substitute);
        }
    }
    row[b_chars.len()]
}
