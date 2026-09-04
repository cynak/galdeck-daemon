//! Colours that can name each other.
//!
//! A theme is only worth having if changing one value moves everything that
//! depends on it, so a colour in this model is either a literal or a `@token`
//! pointing into the theme's palette. Tokens may point at other tokens.
//!
//! References are resolved once, when the theme loads, into a flat table — so
//! nothing on the render path ever hashes a string.

use std::collections::BTreeMap;

use galdeck::Rgb;
use serde::Deserialize;

use crate::diag::{Diagnostic, Diagnostics};

/// How many rounds of token-to-token resolution to allow.
///
/// Every round resolves at least one more token, so a palette that has not
/// settled after this many rounds contains a cycle. Eight is far past any
/// sensible palette depth and keeps the failure cheap to detect.
const MAX_ROUNDS: usize = 8;

/// A colour as written in a config file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorRef {
    Literal(Rgb),
    /// `@name`, resolved against the theme palette.
    Token(String),
}

impl ColorRef {
    pub fn parse(value: &str) -> Result<Self, String> {
        if let Some(name) = value.strip_prefix('@') {
            if name.is_empty() {
                return Err("a colour token needs a name after the @".to_string());
            }
            return Ok(ColorRef::Token(name.to_string()));
        }
        Rgb::from_hex(value)
            .map(ColorRef::Literal)
            .ok_or_else(|| format!("expected #rrggbb or @token, got {value:?}"))
    }
}

impl<'de> Deserialize<'de> for ColorRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        ColorRef::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A theme's named colours, before resolution.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct Palette(pub BTreeMap<String, ColorRef>);

/// A palette with every token resolved to a literal.
#[derive(Clone, Debug, Default)]
pub struct ResolvedPalette {
    colors: BTreeMap<String, Rgb>,
}

impl ResolvedPalette {
    pub fn get(&self, token: &str) -> Option<Rgb> {
        self.colors.get(token).copied()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.colors.keys().map(String::as_str)
    }

    /// Resolve a colour reference, reporting an unknown token.
    pub fn resolve(&self, color: &ColorRef, path: &str, out: &mut Diagnostics) -> Option<Rgb> {
        match color {
            ColorRef::Literal(rgb) => Some(*rgb),
            ColorRef::Token(name) => match self.get(name) {
                Some(rgb) => Some(rgb),
                None => {
                    out.push(
                        Diagnostic::error("E0117", path, format!("unknown colour token @{name}"))
                            .with_help(self.suggest(name)),
                    );
                    None
                }
            },
        }
    }

    fn suggest(&self, unknown: &str) -> String {
        let mut names: Vec<&str> = self.names().collect();
        names.sort_by_key(|name| crate::edit_distance(unknown, name));
        match names.first() {
            Some(best) if crate::edit_distance(unknown, best) <= 3 => {
                format!("did you mean @{best}?")
            }
            _ if names.is_empty() => "this theme defines no colours".to_string(),
            _ => format!("known colours: {}", names.join(", ")),
        }
    }
}

/// Resolve a palette's internal references to a fixed point.
///
/// Each round substitutes every token whose target is already a literal. A
/// palette that stops making progress with tokens left over contains a cycle,
/// and saying which cycle is most of the value of reporting it at all.
pub fn resolve_palette(palette: &Palette, out: &mut Diagnostics) -> ResolvedPalette {
    let mut resolved: BTreeMap<String, Rgb> = BTreeMap::new();
    let mut pending: BTreeMap<&str, &str> = BTreeMap::new();

    for (name, color) in &palette.0 {
        match color {
            ColorRef::Literal(rgb) => {
                resolved.insert(name.clone(), *rgb);
            }
            ColorRef::Token(target) => {
                pending.insert(name.as_str(), target.as_str());
            }
        }
    }

    for _ in 0..MAX_ROUNDS {
        if pending.is_empty() {
            break;
        }
        let settled: Vec<(&str, Rgb)> = pending
            .iter()
            .filter_map(|(name, target)| resolved.get(*target).map(|rgb| (*name, *rgb)))
            .collect();
        if settled.is_empty() {
            break;
        }
        for (name, rgb) in settled {
            resolved.insert(name.to_string(), rgb);
            pending.remove(name);
        }
    }

    for (name, target) in pending {
        // Distinguish "points at nothing" from "points at itself, eventually".
        let known = palette.0.contains_key(target);
        if known {
            out.push(
                Diagnostic::error(
                    "E0116",
                    format!("palette.{name}"),
                    format!("colour token @{name} is part of a cycle"),
                )
                .with_help(describe_cycle(palette, name)),
            );
        } else {
            out.push(Diagnostic::error(
                "E0117",
                format!("palette.{name}"),
                format!("colour token @{name} points at unknown @{target}"),
            ));
        }
    }

    ResolvedPalette { colors: resolved }
}

/// Walk the references from `start` until one repeats, and render the loop.
fn describe_cycle(palette: &Palette, start: &str) -> String {
    let mut seen = Vec::new();
    let mut current = start.to_string();
    loop {
        if seen.contains(&current) {
            seen.push(current);
            let path: Vec<String> = seen.iter().map(|name| format!("@{name}")).collect();
            return path.join(" → ");
        }
        seen.push(current.clone());
        match palette.0.get(&current) {
            Some(ColorRef::Token(next)) => current = next.clone(),
            _ => {
                let path: Vec<String> = seen.iter().map(|name| format!("@{name}")).collect();
                return path.join(" → ");
            }
        }
    }
}
