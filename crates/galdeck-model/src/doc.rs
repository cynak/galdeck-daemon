//! Editing a config file without rewriting it.
//!
//! The shipped example's value is largely its comments, and a hand-written
//! config is the user's own work. Serializing the typed model back out would
//! delete every comment and reorder every table, so edits go through
//! `toml_edit`, which preserves the document and changes only what is asked.
//!
//! An edit is applied to a clone, re-parsed, and re-validated before it is
//! allowed to replace anything. A save that would produce an invalid config is
//! refused rather than written and then complained about.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Value as TomlValue};

use crate::diag::{Diagnostic, Diagnostics};

/// A scalar an edit can set.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
}

impl From<Value> for TomlValue {
    fn from(value: Value) -> Self {
        match value {
            Value::String(s) => TomlValue::from(s),
            Value::Integer(i) => TomlValue::from(i),
            Value::Float(f) => TomlValue::from(f),
            Value::Boolean(b) => TomlValue::from(b),
        }
    }
}

/// One edit.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Patch {
    /// Set a scalar, creating intermediate tables if they are missing.
    Set { path: String, value: Value },
    /// Remove a key. Removing something absent is not an error.
    Remove { path: String },
    /// Append a table to an array of tables, creating the array if needed.
    ///
    /// This is how a new key, encoder or page is added. Written as `[[...]]`
    /// so it reads the way a person would have written it by hand.
    Append {
        path: String,
        fields: std::collections::BTreeMap<String, Value>,
    },
    /// Remove one element of an array of tables.
    RemoveAt { path: String, index: usize },
}

impl Patch {
    pub fn path(&self) -> &str {
        match self {
            Patch::Set { path, .. }
            | Patch::Remove { path }
            | Patch::Append { path, .. }
            | Patch::RemoveAt { path, .. } => path,
        }
    }
}

/// One step of a path: a table key or an array index.
#[derive(Clone, Debug, PartialEq)]
enum Segment {
    Key(String),
    Index(usize),
}

/// Parse `pages[0].keys[2].label` into its steps.
fn parse_path(path: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    for part in path.split('.') {
        if part.is_empty() {
            return Err(format!("empty path segment in {path:?}"));
        }
        let (name, rest) = match part.find('[') {
            Some(at) => part.split_at(at),
            None => (part, ""),
        };
        if !name.is_empty() {
            segments.push(Segment::Key(name.to_string()));
        }
        let mut rest = rest;
        while !rest.is_empty() {
            let close = rest
                .find(']')
                .ok_or_else(|| format!("unclosed [ in {path:?}"))?;
            let index: usize = rest[1..close]
                .parse()
                .map_err(|_| format!("expected a number in {path:?}"))?;
            segments.push(Segment::Index(index));
            rest = &rest[close + 1..];
        }
    }
    if segments.is_empty() {
        return Err(format!("empty path {path:?}"));
    }
    Ok(segments)
}

/// Set a value at a path, creating intermediate tables.
///
/// Recursive rather than a loop because a TOML document alternates between two
/// shapes -- items and tables -- and an array of tables hands back the latter.
fn set_in_item(item: &mut Item, segments: &[Segment], value: TomlValue) -> Result<(), String> {
    let Some((first, rest)) = segments.split_first() else {
        *item = Item::Value(value);
        return Ok(());
    };
    match first {
        Segment::Key(name) => {
            if item.is_none() {
                *item = Item::Table(toml_edit::Table::new());
            }
            let table = item
                .as_table_like_mut()
                .ok_or_else(|| format!("{name} is not inside a table"))?;
            if table.get(name).is_none() {
                // `Item::None` reads as "absent" to toml_edit, so inserting it
                // stores nothing. A real table stands in until the base case
                // replaces it with the value.
                table.insert(name, Item::Table(toml_edit::Table::new()));
            }
            let next = table.get_mut(name).expect("just inserted or already there");
            set_in_item(next, rest, value)
        }
        Segment::Index(index) => {
            let array = item
                .as_array_of_tables_mut()
                .ok_or_else(|| format!("[{index}] is not an array of tables"))?;
            let table = array
                .get_mut(*index)
                .ok_or_else(|| format!("index {index} is past the end"))?;
            let Some((Segment::Key(name), rest)) = rest.split_first() else {
                return Err(format!("[{index}] needs a key after it"));
            };
            if table.get(name).is_none() {
                table.insert(name, Item::Table(toml_edit::Table::new()));
            }
            let next = table.get_mut(name).expect("just inserted or already there");
            set_in_item(next, rest, value)
        }
    }
}

/// Find the table a path's last segment lives in.
fn table_of<'a>(
    item: &'a mut Item,
    segments: &[Segment],
) -> Option<&'a mut dyn toml_edit::TableLike> {
    let Some((first, rest)) = segments.split_first() else {
        return item.as_table_like_mut();
    };
    match first {
        Segment::Key(name) => {
            let table = item.as_table_like_mut()?;
            table_of(table.get_mut(name)?, rest)
        }
        Segment::Index(index) => {
            let array = item.as_array_of_tables_mut()?;
            let table = array.get_mut(*index)?;
            let Some((Segment::Key(name), rest)) = rest.split_first() else {
                return Some(table as &mut dyn toml_edit::TableLike);
            };
            table_of(table.get_mut(name)?, rest)
        }
    }
}

/// Find an array of tables at a path, optionally creating it.
fn array_at<'a>(
    item: &'a mut Item,
    segments: &[Segment],
    create: bool,
) -> Option<&'a mut toml_edit::ArrayOfTables> {
    let Some((first, rest)) = segments.split_first() else {
        return item.as_array_of_tables_mut();
    };
    match first {
        Segment::Key(name) => {
            if create && item.is_none() {
                *item = Item::Table(toml_edit::Table::new());
            }
            let table = item.as_table_like_mut()?;
            if table.get(name).is_none() {
                if !create {
                    return None;
                }
                table.insert(name, Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
            }
            array_at(table.get_mut(name)?, rest, create)
        }
        Segment::Index(index) => {
            let array = item.as_array_of_tables_mut()?;
            let table = array.get_mut(*index)?;
            let (Segment::Key(name), rest) = rest.split_first()? else {
                return None;
            };
            if table.get(name).is_none() {
                if !create {
                    return None;
                }
                table.insert(name, Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
            }
            array_at(table.get_mut(name)?, rest, create)
        }
    }
}

/// A config file, kept as the document it was written as.
#[derive(Clone, Debug)]
pub struct ConfigDocument {
    path: PathBuf,
    doc: DocumentMut,
    /// Bumped on every successful commit.
    ///
    /// A user interface sends back the generation it read, so an edit built
    /// against content that has since changed is refused rather than
    /// silently clobbering whatever replaced it.
    generation: u64,
}

/// An edit that validated, waiting to be written.
#[derive(Clone, Debug)]
pub struct Staged {
    pub text: String,
    pub generation: u64,
}

impl ConfigDocument {
    pub fn parse(path: &Path, text: &str) -> Result<Self, Diagnostics> {
        let doc = text.parse::<DocumentMut>().map_err(|e| {
            let mut diagnostic =
                Diagnostic::error("E0001", path.display().to_string(), e.message().to_string());
            if let Some(span) = e.span() {
                diagnostic = diagnostic.at(span, &crate::diag::LineIndex::new(text));
            }
            diagnostic
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            doc,
            generation: 0,
        })
    }

    pub fn load(path: &Path) -> Result<Self, Diagnostics> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Diagnostic::error("E0002", path.display().to_string(), e.to_string()))?;
        Self::parse(path, &text)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn text(&self) -> String {
        self.doc.to_string()
    }

    /// Apply edits to a copy and hand back the result.
    ///
    /// Nothing is mutated here: the caller validates the produced text against
    /// whatever schema applies and then calls [`ConfigDocument::commit`]. That
    /// split is what lets a user interface show live validation of an edit
    /// nobody has saved.
    pub fn preview(&self, patches: &[Patch], expected: Option<u64>) -> Result<Staged, Diagnostics> {
        let mut out = Diagnostics::new();
        if let Some(expected) = expected {
            if expected != self.generation {
                out.push(
                    Diagnostic::error(
                        "E0150",
                        self.path.display().to_string(),
                        format!(
                            "this file has changed since you read it (generation {}, you have {expected})",
                            self.generation
                        ),
                    )
                    .with_help("re-read it and apply your change again"),
                );
                return Err(out);
            }
        }

        let mut doc = self.doc.clone();
        for patch in patches {
            if let Err(message) = apply_one(&mut doc, patch) {
                out.push(Diagnostic::error(
                    "E0151",
                    patch.path().to_string(),
                    message,
                ));
            }
        }
        if out.has_errors() {
            return Err(out);
        }

        Ok(Staged {
            text: doc.to_string(),
            generation: self.generation + 1,
        })
    }

    /// Accept a previewed edit.
    pub fn commit(&mut self, staged: Staged) -> Result<(), Diagnostics> {
        self.doc = staged.text.parse::<DocumentMut>().map_err(|e| {
            Diagnostics::from_iter([Diagnostic::error(
                "E0001",
                self.path.display().to_string(),
                format!("a staged edit did not re-parse: {}", e.message()),
            )])
        })?;
        self.generation = staged.generation;
        Ok(())
    }

    /// Write the document to disk, durably.
    ///
    /// The previous contents are kept as a `.bak` first, then the new text is
    /// written to a temporary file, flushed, and renamed into place. The
    /// rename is atomic, so a reader never sees a half-written config, and an
    /// interrupted save leaves either the old file or the backup rather than a
    /// truncated one.
    pub fn save(&self) -> std::io::Result<()> {
        use std::io::Write;

        let text = self.doc.to_string();
        if self.path.exists() {
            let backup = self.path.with_extension("toml.bak");
            std::fs::copy(&self.path, &backup)?;
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let staging = self
            .path
            .with_extension(format!("toml.{}", std::process::id()));
        {
            let mut file = std::fs::File::create(&staging)?;
            file.write_all(text.as_bytes())?;
            // Without this the rename can land before the bytes do, leaving an
            // empty file after a power cut.
            file.sync_all()?;
        }
        std::fs::rename(&staging, &self.path)?;

        // The rename itself needs flushing, or the directory entry can be lost.
        if let Some(parent) = self.path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }
}

fn apply_one(doc: &mut DocumentMut, patch: &Patch) -> Result<(), String> {
    match patch {
        Patch::Set { path, value } => {
            let segments = parse_path(path)?;
            set_in_item(doc.as_item_mut(), &segments, value.clone().into())
        }
        Patch::Append { path, fields } => {
            let segments = parse_path(path)?;
            let array = array_at(doc.as_item_mut(), &segments, true)
                .ok_or_else(|| format!("{path:?} is not an array of tables"))?;
            let mut table = toml_edit::Table::new();
            for (name, value) in fields {
                table.insert(name, Item::Value(value.clone().into()));
            }
            array.push(table);
            Ok(())
        }
        Patch::RemoveAt { path, index } => {
            let segments = parse_path(path)?;
            let Some(array) = array_at(doc.as_item_mut(), &segments, false) else {
                return Ok(());
            };
            if *index < array.len() {
                array.remove(*index);
            }
            Ok(())
        }
        Patch::Remove { path } => {
            let segments = parse_path(path)?;
            let (parents, last) = segments.split_at(segments.len() - 1);
            let Segment::Key(name) = &last[0] else {
                return Err("cannot remove an array element by index".to_string());
            };
            // Removing something that is not there is not an error: it is the
            // state the caller asked for.
            if let Some(table) = table_of(doc.as_item_mut(), parents) {
                table.remove(name);
            }
            Ok(())
        }
    }
}
