//! Validation results that a user interface can render.
//!
//! The daemon's `validate()` bails on the first problem with a bare string:
//! no location, no code, no second opinion. That is survivable when the only
//! consumer is a log line, and useless when the consumer is an editor that
//! wants to underline every mistake at once.
//!
//! So validation never bails. It accumulates.

use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Hint,
    Warning,
    Error,
}

/// A byte offset resolved for humans and for the browser.
///
/// `utf16` is the absolute offset in UTF-16 code units. JavaScript's
/// `textarea.selectionStart` counts those rather than bytes, so resolving it
/// here is what makes "click the error, land on the character" work in a
/// config whose labels contain emoji or accents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Loc {
    pub line: u32,
    pub col: u32,
    pub utf16: u32,
}

/// Byte offset to line, column and UTF-16 offset. Built once per file.
pub struct LineIndex {
    /// Byte offset of the start of each line.
    starts: Vec<usize>,
    /// UTF-16 offset of the start of each line.
    utf16_starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0usize];
        let mut utf16_starts = vec![0u32];
        let mut utf16 = 0u32;
        for (offset, ch) in text.char_indices() {
            utf16 += ch.len_utf16() as u32;
            if ch == '\n' {
                starts.push(offset + ch.len_utf8());
                utf16_starts.push(utf16);
            }
        }
        Self {
            starts,
            utf16_starts,
        }
    }

    /// Lines and columns are 1-based, the way every editor reports them.
    pub fn locate(&self, byte: usize) -> Loc {
        // The line whose start is the last one at or before `byte`.
        let line = self.starts.partition_point(|&start| start <= byte) - 1;
        let line_start = self.starts[line];
        Loc {
            line: line as u32 + 1,
            col: (byte - line_start) as u32 + 1,
            utf16: self.utf16_starts[line] + (byte - line_start) as u32,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// A stable code, so a message can be reworded without breaking anyone
    /// who matched on it. `E` blocks loading, `W` and `H` do not.
    pub code: &'static str,
    pub span: Option<Range<usize>>,
    pub start: Option<Loc>,
    pub end: Option<Loc>,
    /// Where in the config this is about, e.g. `pages[0].keys[2].color`.
    pub path: String,
    pub message: String,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, path, message)
    }

    pub fn warning(
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(Severity::Warning, code, path, message)
    }

    pub fn hint(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Hint, code, path, message)
    }

    fn new(
        severity: Severity,
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            code,
            span: None,
            start: None,
            end: None,
            path: path.into(),
            message: message.into(),
            help: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Attach a byte range and resolve it against the source text.
    pub fn at(mut self, span: Range<usize>, index: &LineIndex) -> Self {
        self.start = Some(index.locate(span.start));
        self.end = Some(index.locate(span.end));
        self.span = Some(span);
        self
    }
}

/// An accumulator, so one pass reports every problem rather than the first.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, d: Diagnostic) {
        self.items.push(d);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether anything here blocks loading the config.
    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    /// In source order, so an editor can walk them top to bottom.
    pub fn sorted(mut self) -> Vec<Diagnostic> {
        self.items.sort_by(|a, b| {
            let a_start = a.span.as_ref().map(|s| s.start);
            let b_start = b.span.as_ref().map(|s| s.start);
            a_start.cmp(&b_start).then_with(|| a.code.cmp(b.code))
        });
        self.items
    }

    /// One line per problem, for the log and the CLI.
    pub fn render(&self) -> String {
        self.items
            .iter()
            .map(|d| {
                let where_ = match d.start {
                    Some(loc) => format!("{}:{}", loc.line, loc.col),
                    None => d.path.clone(),
                };
                let help = d
                    .help
                    .as_ref()
                    .map(|h| format!(" ({h})"))
                    .unwrap_or_default();
                format!("{} [{}] {}: {}{}", where_, d.code, d.path, d.message, help)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl FromIterator<Diagnostic> for Diagnostics {
    fn from_iter<T: IntoIterator<Item = Diagnostic>>(iter: T) -> Self {
        Self {
            items: iter.into_iter().collect(),
        }
    }
}
