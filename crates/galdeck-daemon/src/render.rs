//! Turning config entries into images.
//!
//! This is the UX layer's job, so it lives here rather than in the
//! framework: the framework supplies the canvas, primitives, and fonts;
//! this module decides what a key with a label and an icon looks like.

use std::path::Path;

use galdeck::{Align, Button, Canvas, Font, Lcd, Rgb, TextStyle};

/// Height reserved at the bottom of a key for its label.
const LABEL_STRIP: u32 = 36;
const LABEL_SIZE: f32 = 26.0;
const LCD_TEXT_SIZE: f32 = 56.0;
const LCD_BACKGROUND: Rgb = Rgb::new(16, 18, 24);
const LCD_TEXT_COLOR: Rgb = Rgb::new(220, 224, 232);

/// Render one key: background color, optional icon, optional label.
pub fn key(
    background: Rgb,
    icon: Option<&Path>,
    label: Option<&str>,
    font: Option<&Font>,
) -> Canvas {
    let (width, height) = Button::size();
    let mut canvas = Canvas::filled(width, height, background);

    let strip = if label.is_some() { LABEL_STRIP } else { 0 };

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
        let style = TextStyle::new(font, LABEL_SIZE)
            .color(Rgb::WHITE)
            .align(Align::Center)
            .max_width(width - 12);
        canvas.draw_text(label, width as i32 / 2, baseline, &style);
    }

    canvas
}

/// Render the info screen: dark background with centered text.
pub fn lcd(text: &str, font: Option<&Font>) -> Canvas {
    let (width, height) = Lcd::size();
    let (width, height) = (width as u32, height as u32);
    let mut canvas = Canvas::filled(width, height, LCD_BACKGROUND);
    if let Some(font) = font {
        let style = TextStyle::new(font, LCD_TEXT_SIZE)
            .color(LCD_TEXT_COLOR)
            .align(Align::Center)
            .max_width(width - 48);
        canvas.draw_text(text, width as i32 / 2, height as i32 / 2, &style);
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

    #[test]
    fn key_is_the_right_size_and_color() {
        let canvas = key(Rgb::new(10, 20, 30), None, None, None);
        let (width, height) = Button::size();
        assert_eq!((canvas.width(), canvas.height()), (width, height));
        assert_eq!(canvas.pixel(0, 0), Some(Rgb::new(10, 20, 30)));
    }

    #[test]
    fn key_without_a_font_still_renders_its_background() {
        let canvas = key(Rgb::new(1, 2, 3), None, Some("label"), None);
        assert_eq!(canvas.pixel(80, 140), Some(Rgb::new(1, 2, 3)));
    }

    #[test]
    fn key_with_a_missing_icon_falls_back_to_the_background() {
        let canvas = key(
            Rgb::new(9, 9, 9),
            Some(Path::new("/nonexistent/icon.png")),
            None,
            None,
        );
        assert_eq!(canvas.pixel(80, 80), Some(Rgb::new(9, 9, 9)));
    }

    #[test]
    fn lcd_matches_the_screen_size() {
        let canvas = lcd("hello", None);
        let (width, height) = Lcd::size();
        assert_eq!(
            (canvas.width(), canvas.height()),
            (width as u32, height as u32)
        );
        assert_eq!(canvas.pixel(0, 0), Some(LCD_BACKGROUND));
    }
}
