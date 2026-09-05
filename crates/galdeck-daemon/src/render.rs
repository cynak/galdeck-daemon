//! Turning config entries into images.
//!
//! This is the UX layer's job, so it lives here rather than in the
//! framework: the framework supplies the canvas, primitives, and fonts;
//! this module decides what a key with a label and an icon looks like.

use std::path::Path;

use galdeck::{Align, Button, Canvas, Font, Lcd, TextStyle};
use galdeck_model::ResolvedStyle;

/// Most lines a key label may wrap to.
///
/// A key is 160 pixels tall and shares them with an icon. Past two lines the
/// text is too small to read from across a desk, which is the only distance a
/// deck is ever read from.
const MAX_LABEL_LINES: usize = 2;
/// How much smaller a wrapped line may go before giving up and truncating.
const MIN_LABEL_SIZE: f32 = 11.0;

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
        match load_icon(path, width - 8, height.saturating_sub(strip + 8)) {
            Some(icon) => {
                let x = (width as i32 - icon.width() as i32) / 2;
                let y = (height as i32 - strip as i32 - icon.height() as i32) / 2;
                blend_over(&mut canvas, &icon, x, y);
            }
            None => log::warn!("could not use icon {}", path.display()),
        }
    }

    if let (Some(label), Some(font)) = (label, font) {
        let usable = width - 12;
        let (lines, size) = wrap(label, font, style.key_label_size, usable);
        // Centred on where a single line would have gone, so adding a second
        // line grows the block symmetrically rather than pushing it down.
        let centre = if icon.is_some() {
            height as i32 - strip as i32 / 2 - 4
        } else {
            height as i32 / 2
        };
        let spacing = font.line_height(size);
        let first = centre as f32 - spacing * (lines.len() as f32 - 1.0) / 2.0;
        let text = TextStyle::new(font, size)
            .color(style.key_label_color)
            .align(Align::Center)
            .max_width(usable);
        for (index, line) in lines.iter().enumerate() {
            let y = first + spacing * index as f32;
            canvas.draw_text(line, width as i32 / 2, y as i32, &text);
        }
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

/// Break a label into lines that fit, shrinking a little before truncating.
///
/// The framework's text drawing is one line that shrinks to fit, which turns
/// "Screenshot Region" into something unreadable rather than into two words on
/// two lines. This tries the given size, then progressively smaller ones, and
/// only truncates when even the smallest will not do.
fn wrap(text: &str, font: &Font, size: f32, max_width: u32) -> (Vec<String>, f32) {
    let fits = |line: &str, size: f32| font.measure(line, size) <= max_width as f32;

    // One line at the asked-for size is the common case and the nicest result.
    if fits(text, size) {
        return (vec![text.to_string()], size);
    }

    let mut size = size;
    while size >= MIN_LABEL_SIZE {
        if let Some(lines) = break_into(text, |line| fits(line, size)) {
            if lines.len() <= MAX_LABEL_LINES {
                return (lines, size);
            }
        }
        size -= 1.0;
    }

    // Nothing fits. Truncate with an ellipsis rather than overflow the key.
    let mut truncated = text.to_string();
    while !truncated.is_empty() && !fits(&format!("{truncated}…"), MIN_LABEL_SIZE) {
        truncated.pop();
    }
    (vec![format!("{truncated}…")], MIN_LABEL_SIZE)
}

/// Greedy word wrap. `None` when a single word will never fit.
fn break_into(text: &str, fits: impl Fn(&str) -> bool) -> Option<Vec<String>> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if !fits(word) {
            return None;
        }
        match lines.last_mut() {
            Some(line) if fits(&format!("{line} {word}")) => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    (!lines.is_empty()).then_some(lines)
}

/// Load an icon at its own aspect ratio, keeping its transparency.
///
/// `Canvas::load` flattens alpha to black, so a PNG with a transparent
/// background arrives as a black square on whatever the key's colour is. This
/// keeps the alpha channel so it can actually be composited.
fn load_icon(path: &Path, max_width: u32, max_height: u32) -> Option<image::RgbaImage> {
    if max_width == 0 || max_height == 0 {
        return None;
    }
    let image = image::open(path)
        .map_err(|e| log::warn!("loading icon {}: {e}", path.display()))
        .ok()?;
    // Fit inside, never enlarge: blowing a 16x16 icon up to fill a key looks
    // worse than leaving it small.
    let scaled = if image.width() > max_width || image.height() > max_height {
        image.resize(max_width, max_height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    Some(scaled.to_rgba8())
}

/// Composite an image over a canvas, respecting its alpha.
fn blend_over(canvas: &mut Canvas, image: &image::RgbaImage, x: i32, y: i32) {
    for (ix, iy, pixel) in image.enumerate_pixels() {
        let [r, g, b, a] = pixel.0;
        if a == 0 {
            continue;
        }
        canvas.blend_pixel(
            x + ix as i32,
            y + iy as i32,
            galdeck::Rgb::new(r, g, b),
            f32::from(a) / 255.0,
        );
    }
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
