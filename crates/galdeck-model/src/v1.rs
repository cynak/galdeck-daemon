//! The v1 configuration file, as shipped today.
//!
//! Moved out of the daemon unchanged so the schema and its validation can be
//! tested, migrated and served to a user interface without linking the device
//! stack. The struct definitions are deliberately byte-identical to what the
//! daemon carried, so an existing `~/.config/galdeck/config.toml` keeps
//! loading exactly as it did.
//!
//! `Serialize` is deliberately absent. Writing a config back out by
//! serializing these structs would silently delete every comment and reorder
//! every table, and the shipped example's value is largely its comments.
//! Write-back goes through a format-preserving document layer instead.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::diag::{Diagnostic, Diagnostics, LineIndex};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Panel brightness 0-100.
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    /// TTF/OTF font for key labels and the info screen; when unset, a list
    /// of common system font paths is tried.
    #[serde(default)]
    pub font: Option<PathBuf>,
    /// Default info-screen text (pages can override with `lcd_text`).
    #[serde(default)]
    pub lcd_text: Option<String>,
    #[serde(default)]
    pub pages: Vec<Page>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub name: String,
    #[serde(default)]
    pub lcd_text: Option<String>,
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
    #[serde(default)]
    pub encoders: Vec<EncoderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// Key position 0-11, row-major from the top-left of the key grid.
    pub key: u8,
    #[serde(default)]
    pub label: Option<String>,
    /// Background color, `#rrggbb`.
    #[serde(default)]
    pub color: Option<String>,
    /// Icon image path (png/jpeg), scaled to fit.
    #[serde(default)]
    pub image: Option<PathBuf>,
    /// Shell command to run on press.
    #[serde(default)]
    pub exec: Option<String>,
    /// Page to switch to on press (instead of / after exec).
    #[serde(default)]
    pub page: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncoderConfig {
    /// 0 = left, 1 = right.
    pub encoder: u8,
    #[serde(default)]
    pub press: Option<String>,
    /// Shell command per clockwise detent.
    #[serde(default)]
    pub cw: Option<String>,
    #[serde(default)]
    pub ccw: Option<String>,
    /// Ring LED color, `#rrggbb`.
    #[serde(default)]
    pub ring: Option<String>,
}

fn default_brightness() -> u8 {
    60
}

/// What went wrong loading a config file.
#[derive(Debug)]
pub enum LoadError {
    /// The file could not be read.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not valid TOML, or does not match the schema.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// The file parsed but did not validate. Every problem is reported, not
    /// just the first.
    Invalid {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Read { path, source } => {
                write!(f, "reading config {}: {source}", path.display())
            }
            LoadError::Parse { path, source } => {
                write!(f, "parsing config {}: {source}", path.display())
            }
            LoadError::Invalid { path, diagnostics } => {
                write!(f, "{} did not validate:", path.display())?;
                for d in diagnostics {
                    let where_ = match d.start {
                        Some(loc) => format!("{}:{}", loc.line, loc.col),
                        None => d.path.clone(),
                    };
                    write!(f, "\n  {where_} [{}] {}", d.code, d.message)?;
                    if let Some(help) = &d.help {
                        write!(f, " ({help})")?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Read { source, .. } => Some(source),
            LoadError::Parse { source, .. } => Some(source),
            LoadError::Invalid { .. } => None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text).map_err(|e| e.with_path(path))
    }

    /// Parse and validate config text. Split out from [`Config::load`] so the
    /// control API can validate an edit the user has not saved yet.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let config: Config = toml::from_str(text).map_err(ParseError::Parse)?;
        let diagnostics = config.validate(text);
        if diagnostics.has_errors() {
            return Err(ParseError::Invalid(diagnostics.sorted()));
        }
        Ok(config)
    }

    /// Built-in profile used when no config file exists yet.
    pub fn fallback() -> Self {
        Config {
            brightness: default_brightness(),
            font: None,
            lcd_text: Some("galdeck — no config, see galdeck.example.toml".into()),
            pages: vec![Page {
                name: "main".into(),
                lcd_text: None,
                keys: Vec::new(),
                encoders: Vec::new(),
            }],
        }
    }

    /// Check the whole config, reporting every problem rather than the first.
    ///
    /// `text` is the source it was parsed from, used only to turn a located
    /// problem into a line and column. Pass `""` when there is no source.
    pub fn validate(&self, text: &str) -> Diagnostics {
        let index = LineIndex::new(text);
        let mut out = Diagnostics::new();

        if self.brightness > 100 {
            out.push(
                Diagnostic::error("E0101", "brightness", "brightness must be 0-100")
                    .with_help(format!("got {}", self.brightness)),
            );
        }
        if self.pages.is_empty() {
            out.push(Diagnostic::error(
                "E0102",
                "pages",
                "config needs at least one [[pages]] entry",
            ));
        }

        // Duplicate page names were silently tolerated before: the first match
        // won and later ones were unreachable. Say so rather than leaving the
        // user to wonder why their edits do nothing.
        let mut seen_pages: Vec<&str> = Vec::new();
        for (p, page) in self.pages.iter().enumerate() {
            if seen_pages.contains(&page.name.as_str()) {
                out.push(
                    Diagnostic::warning(
                        "W0103",
                        format!("pages[{p}].name"),
                        format!(
                            "duplicate page {:?}; only the first is reachable",
                            page.name
                        ),
                    )
                    .with_help("rename one of them"),
                );
            }
            seen_pages.push(&page.name);

            let mut seen_keys: Vec<u8> = Vec::new();
            for (k, key) in page.keys.iter().enumerate() {
                let path = format!("pages[{p}].keys[{k}]");
                if key.key >= galdeck::Buttons::COUNT {
                    out.push(Diagnostic::error(
                        "E0110",
                        format!("{path}.key"),
                        format!(
                            "page {:?}: key {} out of range 0-{}",
                            page.name,
                            key.key,
                            galdeck::Buttons::COUNT - 1
                        ),
                    ));
                } else if seen_keys.contains(&key.key) {
                    out.push(
                        Diagnostic::warning(
                            "W0111",
                            format!("{path}.key"),
                            format!(
                                "page {:?}: key {} is configured twice; only the first applies",
                                page.name, key.key
                            ),
                        )
                        .with_help("remove one of the entries"),
                    );
                } else {
                    seen_keys.push(key.key);
                }

                if let Some(color) = &key.color {
                    check_color(color, &format!("{path}.color"), &mut out);
                }
                if let Some(target) = &key.page {
                    if !self.pages.iter().any(|p| &p.name == target) {
                        out.push(
                            Diagnostic::error(
                                "E0112",
                                format!("{path}.page"),
                                format!(
                                    "page {:?}: key {} switches to unknown page {target:?}",
                                    page.name, key.key
                                ),
                            )
                            .with_help(did_you_mean(target, &seen_pages_all(self))),
                        );
                    }
                }
                if key.exec.is_none() && key.page.is_none() {
                    out.push(
                        Diagnostic::hint(
                            "H0113",
                            path.clone(),
                            format!(
                                "page {:?}: key {} does nothing when pressed",
                                page.name, key.key
                            ),
                        )
                        .with_help("give it an `exec` or a `page`"),
                    );
                }
            }

            let mut seen_encoders: Vec<u8> = Vec::new();
            for (e, encoder) in page.encoders.iter().enumerate() {
                let path = format!("pages[{p}].encoders[{e}]");
                if encoder.encoder >= galdeck::Encoders::COUNT {
                    out.push(Diagnostic::error(
                        "E0120",
                        format!("{path}.encoder"),
                        format!(
                            "page {:?}: encoder {} out of range 0-{}",
                            page.name,
                            encoder.encoder,
                            galdeck::Encoders::COUNT - 1
                        ),
                    ));
                } else if seen_encoders.contains(&encoder.encoder) {
                    out.push(Diagnostic::warning(
                        "W0121",
                        format!("{path}.encoder"),
                        format!(
                            "page {:?}: encoder {} is configured twice; only the first applies",
                            page.name, encoder.encoder
                        ),
                    ));
                } else {
                    seen_encoders.push(encoder.encoder);
                }

                if let Some(color) = &encoder.ring {
                    check_color(color, &format!("{path}.ring"), &mut out);
                }
            }
        }

        // A page nothing reaches is almost always a typo or a leftover. The
        // first page is the start page, so it is reachable by definition.
        for (p, page) in self.pages.iter().enumerate().skip(1) {
            let reachable = self
                .pages
                .iter()
                .flat_map(|other| &other.keys)
                .any(|key| key.page.as_deref() == Some(page.name.as_str()));
            if !reachable {
                out.push(
                    Diagnostic::hint(
                        "H0104",
                        format!("pages[{p}].name"),
                        format!("no key switches to page {:?}", page.name),
                    )
                    .with_help("bind a key to it, or use `galdeck page` to reach it"),
                );
            }
        }

        let _ = index;
        out
    }
}

fn seen_pages_all(config: &Config) -> Vec<&str> {
    config.pages.iter().map(|p| p.name.as_str()).collect()
}

/// A cheap "did you mean" for a misspelled page name.
fn did_you_mean(target: &str, candidates: &[&str]) -> String {
    let best = candidates
        .iter()
        .map(|c| (edit_distance(target, c), *c))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d);
    match best {
        Some((_, name)) => format!("did you mean {name:?}?"),
        None if candidates.is_empty() => "no pages are defined".to_string(),
        None => format!("known pages: {}", candidates.join(", ")),
    }
}

/// Levenshtein distance, iterative with one row of state.
fn edit_distance(a: &str, b: &str) -> usize {
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

/// Validate a `#rrggbb` color string.
fn check_color(value: &str, path: &str, out: &mut Diagnostics) {
    if galdeck::Rgb::from_hex(value).is_none() {
        out.push(
            Diagnostic::error(
                "E0130",
                path,
                format!("expected a color like #rrggbb, got {value:?}"),
            )
            .with_help("six hex digits, with or without the leading #"),
        );
    }
}

/// What went wrong parsing config text that is not tied to a file yet.
#[derive(Debug)]
pub enum ParseError {
    Parse(toml::de::Error),
    Invalid(Vec<Diagnostic>),
}

impl ParseError {
    fn with_path(self, path: &Path) -> LoadError {
        match self {
            ParseError::Parse(source) => LoadError::Parse {
                path: path.to_path_buf(),
                source,
            },
            ParseError::Invalid(diagnostics) => LoadError::Invalid {
                path: path.to_path_buf(),
                diagnostics,
            },
        }
    }
}

/// Default config file location: `$XDG_CONFIG_HOME/galdeck/config.toml`.
pub fn default_config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("galdeck").join("config.toml")
}
