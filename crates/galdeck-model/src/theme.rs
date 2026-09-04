//! Themes, and the cascade that turns them into pixels' worth of decisions.
//!
//! Every visual constant in the daemon is currently compiled in: the label
//! strip is 36 pixels, labels are 26-point white, the LCD is a dark grey with
//! light grey text. A theme is what makes those the *defaults* rather than the
//! only option.
//!
//! Resolution is a fold from the most general layer to the most specific. The
//! result is total — every field has a value — so nothing downstream has to
//! deal with an `Option`.

use galdeck::Rgb;
use serde::Deserialize;

use crate::color::{ColorRef, Palette, ResolvedPalette};
use crate::diag::Diagnostics;

/// Where a resolved value came from.
///
/// Kept alongside the resolved style so a user interface can say "inherited
/// from theme `nord`" next to a field, and offer to reset it. Threading it
/// through afterwards would mean rewriting the resolver; doing it here costs
/// one extra assignment per field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleSource {
    Builtin,
    Theme,
    Profile,
    Page,
    Cell,
}

/// One layer of style, with everything optional.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleLayer {
    /// Background a key is filled with before its icon and label.
    pub key_bg: Option<ColorRef>,
    pub key_label_color: Option<ColorRef>,
    pub key_label_size: Option<f32>,
    /// Height of the strip at the bottom of a key reserved for its label.
    pub key_label_strip: Option<u32>,
    pub lcd_bg: Option<ColorRef>,
    pub lcd_text_color: Option<ColorRef>,
    pub lcd_text_size: Option<f32>,
    /// Colour an encoder's ring rests at.
    pub ring: Option<ColorRef>,
}

impl StyleLayer {
    pub fn is_empty(&self) -> bool {
        self.key_bg.is_none()
            && self.key_label_color.is_none()
            && self.key_label_size.is_none()
            && self.key_label_strip.is_none()
            && self.lcd_bg.is_none()
            && self.lcd_text_color.is_none()
            && self.lcd_text_size.is_none()
            && self.ring.is_none()
    }
}

/// A total style: no options, nothing left to decide.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedStyle {
    pub key_bg: Rgb,
    pub key_label_color: Rgb,
    pub key_label_size: f32,
    pub key_label_strip: u32,
    pub lcd_bg: Rgb,
    pub lcd_text_color: Rgb,
    pub lcd_text_size: f32,
    pub ring: Rgb,
}

/// Which layer supplied each field of a [`ResolvedStyle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleProvenance {
    pub key_bg: StyleSource,
    pub key_label_color: StyleSource,
    pub key_label_size: StyleSource,
    pub key_label_strip: StyleSource,
    pub lcd_bg: StyleSource,
    pub lcd_text_color: StyleSource,
    pub lcd_text_size: StyleSource,
    pub ring: StyleSource,
}

impl ResolvedStyle {
    /// The look the daemon has had since it shipped, as the bottom layer.
    ///
    /// These were the compile-time constants in `render.rs` and `engine.rs`;
    /// keeping the same values means a config with no theme at all looks
    /// exactly as it did before.
    pub const BUILTIN: ResolvedStyle = ResolvedStyle {
        key_bg: Rgb::new(24, 26, 32),
        key_label_color: Rgb::WHITE,
        key_label_size: 26.0,
        key_label_strip: 36,
        lcd_bg: Rgb::new(16, 18, 24),
        lcd_text_color: Rgb::new(220, 224, 232),
        lcd_text_size: 56.0,
        ring: Rgb::BLACK,
    };

    pub const BUILTIN_PROVENANCE: StyleProvenance = StyleProvenance {
        key_bg: StyleSource::Builtin,
        key_label_color: StyleSource::Builtin,
        key_label_size: StyleSource::Builtin,
        key_label_strip: StyleSource::Builtin,
        lcd_bg: StyleSource::Builtin,
        lcd_text_color: StyleSource::Builtin,
        lcd_text_size: StyleSource::Builtin,
        ring: StyleSource::Builtin,
    };
}

/// Fold a stack of layers onto the built-in defaults.
///
/// Layers are given most-general first. A later layer overrides an earlier
/// one field by field, so a page may restyle one thing without restating the
/// theme.
pub fn resolve(
    layers: &[(StyleSource, &StyleLayer)],
    palette: &ResolvedPalette,
    path: &str,
    out: &mut Diagnostics,
) -> (ResolvedStyle, StyleProvenance) {
    let mut style = ResolvedStyle::BUILTIN;
    let mut from = ResolvedStyle::BUILTIN_PROVENANCE;

    for (source, layer) in layers {
        let mut color =
            |value: &Option<ColorRef>, field: &str, slot: &mut Rgb, at: &mut StyleSource| {
                if let Some(reference) = value {
                    if let Some(rgb) = palette.resolve(reference, &format!("{path}.{field}"), out) {
                        *slot = rgb;
                        *at = *source;
                    }
                }
            };
        color(&layer.key_bg, "key_bg", &mut style.key_bg, &mut from.key_bg);
        color(
            &layer.key_label_color,
            "key_label_color",
            &mut style.key_label_color,
            &mut from.key_label_color,
        );
        color(&layer.lcd_bg, "lcd_bg", &mut style.lcd_bg, &mut from.lcd_bg);
        color(
            &layer.lcd_text_color,
            "lcd_text_color",
            &mut style.lcd_text_color,
            &mut from.lcd_text_color,
        );
        color(&layer.ring, "ring", &mut style.ring, &mut from.ring);

        if let Some(size) = layer.key_label_size {
            style.key_label_size = size;
            from.key_label_size = *source;
        }
        if let Some(strip) = layer.key_label_strip {
            style.key_label_strip = strip;
            from.key_label_strip = *source;
        }
        if let Some(size) = layer.lcd_text_size {
            style.lcd_text_size = size;
            from.lcd_text_size = *source;
        }
    }

    (style, from)
}

/// A theme file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    /// Human-readable name. The theme's id is its filename stem.
    #[serde(default)]
    pub name: Option<String>,
    /// Another theme to start from. Followed to a bounded depth.
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub palette: Palette,
    #[serde(default)]
    pub style: StyleLayer,
}

/// How deep an `extends` chain may go.
///
/// Deep enough for "mine extends nord extends base", shallow enough that a
/// cycle is caught immediately rather than by a stack overflow.
pub const MAX_EXTENDS_DEPTH: usize = 4;
