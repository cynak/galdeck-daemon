//! Loading a config directory, and migrating an old one.
//!
//! A v1 `config.toml` is migrated in memory on the way in. Nothing on disk
//! changes until the user asks — an upgrade that silently rewrites the file
//! whose comments are the user's own work is not an upgrade.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::color::{resolve_palette, ColorRef, Palette, ResolvedPalette};
use crate::diag::{Diagnostic, Diagnostics, LineIndex};
use crate::theme::{
    resolve, ResolvedStyle, StyleLayer, StyleProvenance, StyleSource, MAX_EXTENDS_DEPTH,
};
use crate::v1;
use crate::v2::{EncoderConfig, Global, KeyConfig, Page, Profile, Workspace, CURRENT_VERSION};

/// Where a config directory lives: `$XDG_CONFIG_HOME/galdeck`.
pub fn default_config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("galdeck")
}

impl Workspace {
    /// Load a config directory, or migrate a v1 file found in its place.
    ///
    /// Returns whatever could be loaded alongside everything wrong with it, so
    /// a user interface can show all the problems at once rather than one per
    /// save.
    pub fn load(dir: &Path) -> (Option<Workspace>, Vec<Diagnostic>) {
        Self::load_with_overrides(dir, &BTreeMap::new())
    }

    /// Load a config directory, substituting the text of named files.
    ///
    /// This is what makes live validation honest: an edit is checked against
    /// the whole workspace it would create, so "this theme no longer defines
    /// @accent, and three keys reference it" is caught before saving rather
    /// than after.
    pub fn load_with_overrides(
        dir: &Path,
        overrides: &BTreeMap<String, String>,
    ) -> (Option<Workspace>, Vec<Diagnostic>) {
        let mut out = Diagnostics::new();

        // An existing single-file config keeps working untouched.
        let legacy = dir.join("config.toml");
        if !dir.join("galdeck.toml").exists() && legacy.exists() {
            return match std::fs::read_to_string(&legacy) {
                Ok(text) => match v1::Config::parse(&text) {
                    Ok(config) => {
                        let workspace = Workspace::from_v1(&config);
                        out.push(Diagnostic::hint(
                            "H0100",
                            "config.toml",
                            "loaded a version 1 config; run `galdeck config migrate` to split it into profiles and themes",
                        ));
                        let mut all = workspace.validate();
                        for d in out.sorted() {
                            all.push(d);
                        }
                        (Some(workspace), all.sorted())
                    }
                    Err(v1::ParseError::Parse(e)) => {
                        out.push(Diagnostic::error("E0001", "config.toml", e.to_string()));
                        (None, out.sorted())
                    }
                    Err(v1::ParseError::Invalid(diagnostics)) => (None, diagnostics),
                },
                Err(e) => {
                    out.push(Diagnostic::error("E0002", "config.toml", e.to_string()));
                    (None, out.sorted())
                }
            };
        }

        let global = match read_toml::<Global>(
            &dir.join("galdeck.toml"),
            "galdeck.toml",
            overrides.get("galdeck.toml").map(String::as_str),
            &mut out,
        ) {
            Some(global) => global,
            None => return (None, out.sorted()),
        };

        if global.version > CURRENT_VERSION {
            out.push(
                Diagnostic::error(
                    "E0003",
                    "galdeck.toml.version",
                    format!(
                        "this config is version {}, but this build understands up to {CURRENT_VERSION}",
                        global.version
                    ),
                )
                .with_help("it was probably written by a newer galdeck; upgrading should fix it"),
            );
            return (None, out.sorted());
        }

        let profiles = read_dir_of("profiles", dir, overrides, &mut out);
        let themes = read_dir_of("themes", dir, overrides, &mut out);

        let workspace = Workspace {
            global,
            profiles,
            themes,
        };
        let mut all = workspace.validate();
        for d in out.sorted() {
            all.push(d);
        }
        (Some(workspace), all.sorted())
    }

    /// Turn a version 1 config into the version 2 shape, in memory.
    ///
    /// Everything lands in one profile called `default` with no theme, because
    /// a v1 config expressed its colours per key and per encoder — which is
    /// exactly what a cell-level style layer is.
    pub fn from_v1(config: &v1::Config) -> Workspace {
        let pages = config
            .pages
            .iter()
            .map(|page| Page {
                id: page.name.clone(),
                lcd_text: page.lcd_text.clone(),
                style: StyleLayer::default(),
                keys: page
                    .keys
                    .iter()
                    .map(|key| KeyConfig {
                        key: key.key,
                        label: key.label.clone(),
                        icon: key.image.clone(),
                        exec: key.exec.clone(),
                        page: key.page.clone(),
                        profile: None,
                        back: false,
                        animation: None,
                        widget: None,
                        style: StyleLayer {
                            // v1's `color` was the key background.
                            key_bg: key.color.as_deref().and_then(literal),
                            ..StyleLayer::default()
                        },
                    })
                    .collect(),
                encoders: page
                    .encoders
                    .iter()
                    .map(|encoder| EncoderConfig {
                        encoder: encoder.encoder,
                        press: encoder.press.clone(),
                        cw: encoder.cw.clone(),
                        ccw: encoder.ccw.clone(),
                        style: StyleLayer {
                            ring: encoder.ring.as_deref().and_then(literal),
                            ..StyleLayer::default()
                        },
                        animation: None,
                    })
                    .collect(),
            })
            .collect();

        let profile = Profile {
            name: Some("Default".to_string()),
            theme: None,
            home: config.pages.first().map(|page| page.name.clone()),
            style: StyleLayer::default(),
            pages,
        };

        Workspace {
            global: Global {
                version: CURRENT_VERSION,
                brightness: config.brightness,
                font: config.font.clone(),
                profile: Some("default".to_string()),
            },
            profiles: BTreeMap::from([("default".to_string(), profile)]),
            themes: BTreeMap::new(),
        }
    }

    /// A theme with its `extends` chain folded in, and its palette resolved.
    pub fn theme_for(
        &self,
        id: Option<&str>,
        out: &mut Diagnostics,
    ) -> (StyleLayer, ResolvedPalette) {
        let Some(id) = id else {
            return (StyleLayer::default(), ResolvedPalette::default());
        };
        let Some(theme) = self.themes.get(id) else {
            out.push(Diagnostic::error(
                "E0115",
                "theme",
                format!("unknown theme {id:?}"),
            ));
            return (StyleLayer::default(), ResolvedPalette::default());
        };

        // Walk the chain from this theme up to its most distant ancestor, then
        // fold back down so the nearest theme wins.
        let mut chain = vec![theme];
        let mut seen = vec![id.to_string()];
        let mut current = theme;
        while let Some(parent_id) = current.extends.as_deref() {
            if seen.iter().any(|s| s == parent_id) {
                out.push(Diagnostic::error(
                    "E0114",
                    format!("themes.{parent_id}.extends"),
                    format!("theme {parent_id:?} extends itself, directly or otherwise"),
                ));
                break;
            }
            if chain.len() >= MAX_EXTENDS_DEPTH {
                out.push(Diagnostic::error(
                    "E0113",
                    format!("themes.{id}.extends"),
                    format!("theme inheritance is deeper than {MAX_EXTENDS_DEPTH} levels"),
                ));
                break;
            }
            let Some(parent) = self.themes.get(parent_id) else {
                out.push(Diagnostic::error(
                    "E0115",
                    format!("themes.{}.extends", seen.last().expect("non-empty")),
                    format!("unknown theme {parent_id:?}"),
                ));
                break;
            };
            seen.push(parent_id.to_string());
            chain.push(parent);
            current = parent;
        }

        let mut palette = Palette::default();
        let mut style = StyleLayer::default();
        for theme in chain.iter().rev() {
            for (name, color) in &theme.palette.0 {
                palette.0.insert(name.clone(), color.clone());
            }
            overlay(&mut style, &theme.style);
        }

        (style, resolve_palette(&palette, out))
    }

    /// Resolve the style for one cell.
    ///
    /// Layers run from the most general to the most specific, so a page may
    /// restyle one field without restating the theme.
    #[allow(clippy::too_many_arguments)]
    pub fn style_for(
        &self,
        theme: &StyleLayer,
        palette: &ResolvedPalette,
        profile: &Profile,
        page: &Page,
        cell: Option<&StyleLayer>,
        path: &str,
        out: &mut Diagnostics,
    ) -> (ResolvedStyle, StyleProvenance) {
        let mut layers = vec![
            (StyleSource::Theme, theme),
            (StyleSource::Profile, &profile.style),
            (StyleSource::Page, &page.style),
        ];
        if let Some(cell) = cell {
            layers.push((StyleSource::Cell, cell));
        }
        resolve(&layers, palette, path, out)
    }

    /// Everything wrong with this workspace.
    pub fn validate(&self) -> Diagnostics {
        let mut out = Diagnostics::new();

        if self.global.brightness > 100 {
            out.push(
                Diagnostic::error("E0101", "brightness", "brightness must be 0-100")
                    .with_help(format!("got {}", self.global.brightness)),
            );
        }
        if self.profiles.is_empty() {
            out.push(Diagnostic::error(
                "E0104",
                "profiles",
                "no profiles found; there is nothing to show",
            ));
        }
        if let Some(start) = self.global.profile.as_deref() {
            if !self.profiles.contains_key(start) {
                out.push(Diagnostic::error(
                    "E0105",
                    "profile",
                    format!("start profile {start:?} does not exist"),
                ));
            }
        }

        for (id, profile) in &self.profiles {
            let at = |what: &str| format!("profiles.{id}.{what}");
            if profile.pages.is_empty() {
                out.push(Diagnostic::error(
                    "E0102",
                    at("pages"),
                    "a profile needs at least one page",
                ));
            }
            if let Some(theme) = profile.theme.as_deref() {
                if !self.themes.contains_key(theme) {
                    out.push(Diagnostic::error(
                        "E0115",
                        at("theme"),
                        format!("unknown theme {theme:?}"),
                    ));
                }
            }
            if let Some(home) = profile.home.as_deref() {
                if profile.page(home).is_none() {
                    out.push(Diagnostic::error(
                        "E0106",
                        at("home"),
                        format!("home page {home:?} does not exist"),
                    ));
                }
            }

            let mut seen_pages: Vec<&str> = Vec::new();
            for (p, page) in profile.pages.iter().enumerate() {
                let at = |what: &str| format!("profiles.{id}.pages[{p}].{what}");
                if seen_pages.contains(&page.id.as_str()) {
                    out.push(Diagnostic::warning(
                        "W0103",
                        at("id"),
                        format!("duplicate page {:?}; only the first is reachable", page.id),
                    ));
                }
                seen_pages.push(&page.id);

                let mut seen_keys: Vec<u8> = Vec::new();
                for (k, key) in page.keys.iter().enumerate() {
                    let at = |what: &str| format!("profiles.{id}.pages[{p}].keys[{k}].{what}");
                    if key.key >= galdeck::Buttons::COUNT {
                        out.push(Diagnostic::error(
                            "E0110",
                            at("key"),
                            format!(
                                "key {} out of range 0-{}",
                                key.key,
                                galdeck::Buttons::COUNT - 1
                            ),
                        ));
                    } else if seen_keys.contains(&key.key) {
                        out.push(Diagnostic::warning(
                            "W0111",
                            at("key"),
                            format!(
                                "key {} is configured twice; only the first applies",
                                key.key
                            ),
                        ));
                    } else {
                        seen_keys.push(key.key);
                    }

                    if let Some(target) = key.page.as_deref() {
                        if profile.page(target).is_none() {
                            out.push(
                                Diagnostic::error(
                                    "E0112",
                                    at("page"),
                                    format!("switches to unknown page {target:?}"),
                                )
                                .with_help(nearest(target, &seen_pages_of(profile))),
                            );
                        }
                    }
                    if let Some(target) = key.profile.as_deref() {
                        if !self.profiles.contains_key(target) {
                            out.push(
                                Diagnostic::error(
                                    "E0107",
                                    at("profile"),
                                    format!("switches to unknown profile {target:?}"),
                                )
                                .with_help(nearest(
                                    target,
                                    &self.profiles.keys().map(String::as_str).collect::<Vec<_>>(),
                                )),
                            );
                        }
                    }
                    if let Some(animation) = &key.animation {
                        if animation.kind.is_ring_only() {
                            out.push(
                                Diagnostic::error(
                                    "E0140",
                                    at("animation.kind"),
                                    format!("{:?} only works on an encoder ring", animation.kind),
                                )
                                .with_help("try \"pulse\", \"breathe\" or \"blink\""),
                            );
                        }
                        check_period(animation, &at("animation.period_ms"), &mut out);
                    }
                    if let Some(widget) = &key.widget {
                        if widget.kind == crate::widget::WidgetKind::Command
                            && widget.command.as_deref().unwrap_or_default().is_empty()
                        {
                            out.push(Diagnostic::error(
                                "E0142",
                                at("widget.command"),
                                "a command widget needs a `command` to run",
                            ));
                        }
                        if widget.kind != crate::widget::WidgetKind::Command
                            && widget.command.is_some()
                        {
                            out.push(Diagnostic::warning(
                                "W0143",
                                at("widget.command"),
                                format!("{:?} widgets ignore `command`", widget.kind),
                            ));
                        }
                    }
                    if !key.is_bound() {
                        out.push(
                            Diagnostic::hint("H0113", at(""), "this key does nothing when pressed")
                                .with_help(
                                    "give it an `exec`, a `page`, a `profile`, or `back = true`",
                                ),
                        );
                    }
                }

                let mut seen_encoders: Vec<u8> = Vec::new();
                for (e, encoder) in page.encoders.iter().enumerate() {
                    let at = |what: &str| format!("profiles.{id}.pages[{p}].encoders[{e}].{what}");
                    if encoder.encoder >= galdeck::Encoders::COUNT {
                        out.push(Diagnostic::error(
                            "E0120",
                            at("encoder"),
                            format!(
                                "encoder {} out of range 0-{}",
                                encoder.encoder,
                                galdeck::Encoders::COUNT - 1
                            ),
                        ));
                    } else if seen_encoders.contains(&encoder.encoder) {
                        out.push(Diagnostic::warning(
                            "W0121",
                            at("encoder"),
                            format!(
                                "encoder {} is configured twice; only the first applies",
                                encoder.encoder
                            ),
                        ));
                    } else {
                        seen_encoders.push(encoder.encoder);
                    }

                    if let Some(animation) = &encoder.animation {
                        check_period(animation, &at("animation.period_ms"), &mut out);
                    }
                }
            }
        }

        // Themes are checked by resolving them, which is what surfaces palette
        // cycles and unknown tokens in the palette itself.
        for id in self.themes.keys() {
            let _ = self.theme_for(Some(id), &mut out);
        }

        // Resolving every cell's style is the only thing that reaches tokens
        // used in a style layer rather than in a palette. It is also a free
        // check that the whole cascade holds together.
        for (id, profile) in &self.profiles {
            let mut theme_diagnostics = Diagnostics::new();
            let (theme, palette) = self.theme_for(profile.theme.as_deref(), &mut theme_diagnostics);
            // Already reported above; resolving again would duplicate them.
            drop(theme_diagnostics);

            for (p, page) in profile.pages.iter().enumerate() {
                let at = |what: &str| format!("profiles.{id}.pages[{p}].{what}");
                let _ = self.style_for(
                    &theme,
                    &palette,
                    profile,
                    page,
                    None,
                    &at("style"),
                    &mut out,
                );
                for (k, key) in page.keys.iter().enumerate() {
                    let path = format!("profiles.{id}.pages[{p}].keys[{k}].style");
                    let _ = self.style_for(
                        &theme,
                        &palette,
                        profile,
                        page,
                        Some(&key.style),
                        &path,
                        &mut out,
                    );
                }
                for (e, encoder) in page.encoders.iter().enumerate() {
                    let path = format!("profiles.{id}.pages[{p}].encoders[{e}].style");
                    let _ = self.style_for(
                        &theme,
                        &palette,
                        profile,
                        page,
                        Some(&encoder.style),
                        &path,
                        &mut out,
                    );
                }
            }
        }

        out
    }
}

/// A period outside what the panel can show is clamped rather than refused,
/// but silently clamping a value the user typed is how a config stops meaning
/// what it says.
fn check_period(animation: &crate::animation::Animation, path: &str, out: &mut Diagnostics) {
    use crate::animation::{MAX_PERIOD_MS, MIN_PERIOD_MS};
    if animation.period_ms < MIN_PERIOD_MS || animation.period_ms > MAX_PERIOD_MS {
        out.push(
            Diagnostic::warning(
                "W0141",
                path,
                format!(
                    "period {} ms is outside {MIN_PERIOD_MS}-{MAX_PERIOD_MS} and will be clamped to {}",
                    animation.period_ms,
                    animation.period_ms()
                ),
            )
            .with_help("below the minimum it reads as a flicker rather than motion"),
        );
    }
}

fn seen_pages_of(profile: &Profile) -> Vec<&str> {
    profile.pages.iter().map(|page| page.id.as_str()).collect()
}

fn nearest(target: &str, candidates: &[&str]) -> String {
    let best = candidates
        .iter()
        .map(|c| (crate::edit_distance(target, c), *c))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d);
    match best {
        Some((_, name)) => format!("did you mean {name:?}?"),
        None if candidates.is_empty() => "none are defined".to_string(),
        None => format!("known: {}", candidates.join(", ")),
    }
}

/// Copy every field the overlay sets onto the base.
fn overlay(base: &mut StyleLayer, over: &StyleLayer) {
    if over.key_bg.is_some() {
        base.key_bg = over.key_bg.clone();
    }
    if over.key_label_color.is_some() {
        base.key_label_color = over.key_label_color.clone();
    }
    if over.key_label_size.is_some() {
        base.key_label_size = over.key_label_size;
    }
    if over.key_label_strip.is_some() {
        base.key_label_strip = over.key_label_strip;
    }
    if over.lcd_bg.is_some() {
        base.lcd_bg = over.lcd_bg.clone();
    }
    if over.lcd_text_color.is_some() {
        base.lcd_text_color = over.lcd_text_color.clone();
    }
    if over.lcd_text_size.is_some() {
        base.lcd_text_size = over.lcd_text_size;
    }
    if over.ring.is_some() {
        base.ring = over.ring.clone();
    }
}

fn literal(value: &str) -> Option<ColorRef> {
    ColorRef::parse(value).ok()
}

fn read_toml<T: serde::de::DeserializeOwned>(
    path: &Path,
    label: &str,
    override_text: Option<&str>,
    out: &mut Diagnostics,
) -> Option<T> {
    let text = match override_text {
        Some(text) => text.to_string(),
        None => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                out.push(Diagnostic::error(
                    "E0002",
                    label,
                    format!("reading {}: {e}", path.display()),
                ));
                return None;
            }
        },
    };
    match toml::from_str::<T>(&text) {
        Ok(value) => Some(value),
        Err(e) => {
            let index = LineIndex::new(&text);
            let mut diagnostic = Diagnostic::error("E0001", label, e.message().to_string());
            if let Some(span) = e.span() {
                diagnostic = diagnostic.at(span, &index);
            }
            out.push(diagnostic);
            None
        }
    }
}

/// Read every `.toml` in a directory, keyed by filename stem.
fn read_dir_of<T: serde::de::DeserializeOwned>(
    label: &str,
    root: &Path,
    overrides: &BTreeMap<String, String>,
    out: &mut Diagnostics,
) -> BTreeMap<String, T> {
    let dir = root.join(label);
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect(),
        // A missing directory is not an error: a config may have no themes.
        Err(_) => Vec::new(),
    };
    // An override may name a file that does not exist on disk yet.
    for name in overrides.keys() {
        if let Some(stem) = name.strip_prefix(&format!("{label}/")) {
            let path = dir.join(stem);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    // Sorted so loading is deterministic, which keeps diagnostics stable.
    paths.sort();

    let mut found = BTreeMap::new();
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let relative = format!("{label}/{stem}.toml");
        let at = format!("{label}.{stem}");
        if let Some(value) = read_toml::<T>(
            &path,
            &at,
            overrides.get(&relative).map(String::as_str),
            out,
        ) {
            found.insert(stem.to_string(), value);
        }
    }
    found
}

/// Every configuration file in a directory, by its path relative to it.
pub fn config_file_names(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if dir.join("galdeck.toml").is_file() {
        names.push("galdeck.toml".to_string());
    }
    for sub in ["profiles", "themes"] {
        let Ok(entries) = std::fs::read_dir(dir.join(sub)) else {
            continue;
        };
        let mut found: Vec<String> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .filter_map(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| format!("{sub}/{n}"))
            })
            .collect();
        found.sort();
        names.extend(found);
    }
    names
}
