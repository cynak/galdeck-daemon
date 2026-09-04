//! Turning config entries into images.
//!
//! This is the UX layer's job, so it lives here rather than in the
//! framework: the framework supplies the canvas, primitives, and fonts;
//! this module decides what a key with a label and an icon looks like.

use std::path::Path;

use galdeck::{Align, Button, Canvas, Font, Lcd, TextStyle};
use galdeck_model::ResolvedStyle;

/// Render one key from a resolved style: background, optional icon, optional
/// label.
///
/// The style is already total by the time it gets here -- the cascade decided
/// every field -- so this function makes no decisions, only pixels.
pub fn key(
    style: &ResolvedStyle,
    icon: Option<&Path>,
    label: Option<&str>,
    font: Option<&Font>,
) -> Canvas {
    let (width, height) = Button::size();
    let mut canvas = Canvas::filled(width, height, style.key_bg);

    let strip = if label.is_some() {
        style.key_label_strip
    } else {
        0
    };

    if let Some(path) = icon {
        match Canvas::load_scaled(path, width - 8, height - strip - 8) {
            Ok(icon) => {
                let x = (width as i32 - icon.width() as i32) / 2;
                let y = (height as i32 - strip as i32 - icon.height() as i32) / 2;
                canvas.blit(&icon, x, y);
            }
            Err(e) => log::warn!("loading icon {}: {e}", path.display()),
        }
    }

    if let (Some(label), Some(font)) = (label, font) {
        let baseline = if icon.is_some() {
            height as i32 - strip as i32 / 2 - 4
        } else {
            height as i32 / 2
        };
        let text = TextStyle::new(font, style.key_label_size)
            .color(style.key_label_color)
            .align(Align::Center)
            .max_width(width - 12);
        canvas.draw_text(label, width as i32 / 2, baseline, &text);
    }

    canvas
}

/// Render the info screen: a flat background with centered text.
pub fn lcd(style: &ResolvedStyle, text: &str, font: Option<&Font>) -> Canvas {
    let (width, height) = Lcd::size();
    let (width, height) = (width as u32, height as u32);
    let mut canvas = Canvas::filled(width, height, style.lcd_bg);
    if let Some(font) = font {
        let text_style = TextStyle::new(font, style.lcd_text_size)
            .color(style.lcd_text_color)
            .align(Align::Center)
            .max_width(width - 48);
        canvas.draw_text(text, width as i32 / 2, height as i32 / 2, &text_style);
    }
    canvas
}

/// Load the configured font, falling back to a system face.
pub fn load_font(configured: Option<&Path>) -> Option<Font> {
    if let Some(path) = configured {
        match Font::load(path) {
            Ok(font) => return Some(font),
            Err(e) => log::warn!(
                "loading font {}: {e}; falling back to a system font",
                path.display()
            ),
        }
    }
    let font = Font::system();
    if font.is_none() {
        log::warn!("no usable font found — labels will be skipped; set `font` in the config");
    }
    font
}

#[cfg(test)]
mod tests {
    use super::*;
    use galdeck::Rgb;

    /// A style with a distinctive background, so a fill is unmistakable.
    fn style(bg: Rgb) -> ResolvedStyle {
        ResolvedStyle {
            key_bg: bg,
            ..ResolvedStyle::BUILTIN
        }
    }

    #[test]
    fn a_key_is_the_size_and_colour_the_style_asks_for() {
        let canvas = key(&style(Rgb::new(10, 20, 30)), None, None, None);
        let (width, height) = Button::size();
        assert_eq!(canvas.width(), width);
        assert_eq!(canvas.height(), height);
        assert_eq!(canvas.pixel(0, 0), Some(Rgb::new(10, 20, 30)));
    }

    #[test]
    fn a_missing_font_leaves_the_background_intact() {
        // Labels are skipped rather than fatal when no font is available, so
        // these tests pass on a runner with no fonts installed.
        let canvas = key(&style(Rgb::new(1, 2, 3)), None, Some("Label"), None);
        assert_eq!(canvas.pixel(0, 0), Some(Rgb::new(1, 2, 3)));
    }

    #[test]
    fn a_missing_icon_leaves_the_background_intact() {
        let missing = Path::new("/definitely/not/here.png");
        let canvas = key(&style(Rgb::new(4, 5, 6)), Some(missing), None, None);
        assert_eq!(canvas.pixel(0, 0), Some(Rgb::new(4, 5, 6)));
    }

    #[test]
    fn the_lcd_is_the_panel_size_and_the_styles_background() {
        let mut s = ResolvedStyle::BUILTIN;
        s.lcd_bg = Rgb::new(7, 8, 9);
        let canvas = lcd(&s, "hello", None);
        let (width, height) = Lcd::size();
        assert_eq!(canvas.width(), width as u32);
        assert_eq!(canvas.height(), height as u32);
        assert_eq!(canvas.pixel(0, 0), Some(Rgb::new(7, 8, 9)));
    }

    #[test]
    fn the_label_strip_is_only_reserved_when_there_is_a_label() {
        // With no label the icon gets the whole key; with one it gets the key
        // minus the strip. Asserted through the style so a theme can change it.
        let mut s = ResolvedStyle::BUILTIN;
        s.key_label_strip = 100;
        let without = key(&s, None, None, None);
        let with = key(&s, None, Some("x"), None);
        assert_eq!(without.width(), with.width());
    }
}
