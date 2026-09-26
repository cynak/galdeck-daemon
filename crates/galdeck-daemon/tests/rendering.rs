//! What a key actually looks like.

use galdeck::{Font, Rgb};
use galdeck_daemon::render;
use galdeck_model::ResolvedStyle;

/// A font, or `None` on a machine with none installed — in which case the
/// text tests have nothing to say and skip.
fn font() -> Option<Font> {
    Font::system()
}

fn style(bg: Rgb) -> ResolvedStyle {
    ResolvedStyle {
        key_bg: bg,
        ..ResolvedStyle::BUILTIN
    }
}

/// Write a PNG with a transparent border and an opaque centre.
fn transparent_png(path: &std::path::Path, size: u32) {
    let mut image = image::RgbaImage::new(size, size);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let inside = x > size / 4 && x < size * 3 / 4 && y > size / 4 && y < size * 3 / 4;
        *pixel = image::Rgba(if inside {
            [255, 0, 0, 255]
        } else {
            [0, 0, 0, 0]
        });
    }
    image.save(path).expect("writing the test icon");
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("galdeck-render-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn a_transparent_icon_does_not_paint_a_black_box() {
    // `Canvas::load` flattens alpha to black, so a PNG with a transparent
    // background used to arrive as a black square sitting on the key colour.
    let path = scratch("transparent.png");
    transparent_png(&path, 64);

    let background = Rgb::new(0x20, 0x60, 0xa0);
    let canvas = render::key(
        galdeck::Button::size(),
        &style(background),
        Some(&path),
        None,
        None,
    );

    // The corners are where the icon's transparency is, so they must still be
    // the key's background rather than black.
    assert_eq!(canvas.pixel(0, 0), Some(background));
    let (width, height) = (canvas.width(), canvas.height());
    assert_eq!(
        canvas.pixel(width as i32 - 1, height as i32 - 1),
        Some(background)
    );

    // And the opaque middle of the icon did land.
    let centre = canvas.pixel(width as i32 / 2, height as i32 / 2).unwrap();
    assert!(
        centre.r > centre.b,
        "the icon's red centre should be visible, got {centre:?}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_small_icon_is_not_blown_up_to_fill_the_key() {
    // Enlarging a 16x16 icon to 160x160 looks worse than leaving it small.
    let path = scratch("small.png");
    transparent_png(&path, 16);
    let canvas = render::key(
        galdeck::Button::size(),
        &style(Rgb::BLACK),
        Some(&path),
        None,
        None,
    );

    // Well outside where a 16px icon centred on a 160px key could reach.
    assert_eq!(canvas.pixel(20, 20), Some(Rgb::BLACK));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_missing_icon_leaves_the_key_alone() {
    let missing = std::path::Path::new("/definitely/not/here.png");
    let background = Rgb::new(9, 9, 9);
    let canvas = render::key(
        galdeck::Button::size(),
        &style(background),
        Some(missing),
        None,
        None,
    );
    assert_eq!(canvas.pixel(0, 0), Some(background));
}

#[test]
fn a_long_label_wraps_rather_than_shrinking_to_nothing() {
    let Some(font) = font() else {
        return;
    };
    // Two words that will not fit on one line at 26pt across 148 usable pixels.
    let wrapped = render::key(
        galdeck::Button::size(),
        &style(Rgb::BLACK),
        None,
        Some("Screenshot Region"),
        Some(&font),
    );
    let single = render::key(
        galdeck::Button::size(),
        &style(Rgb::BLACK),
        None,
        Some("Screenshot"),
        Some(&font),
    );

    // The wrapped label occupies more vertical space than the single word,
    // which is what "it wrapped" looks like from the outside.
    let rows_with_ink = |canvas: &galdeck::Canvas| {
        (0..canvas.height())
            .filter(|y| {
                (0..canvas.width()).any(|x| {
                    canvas
                        .pixel(x as i32, *y as i32)
                        .is_some_and(|p| p != Rgb::BLACK)
                })
            })
            .count()
    };
    assert!(
        rows_with_ink(&wrapped) > rows_with_ink(&single),
        "the two-word label should take more rows"
    );
}

#[test]
fn an_unbreakable_label_is_truncated_rather_than_overflowing() {
    let Some(font) = font() else {
        return;
    };
    let canvas = render::key(
        galdeck::Button::size(),
        &style(Rgb::BLACK),
        None,
        Some("Supercalifragilisticexpialidocious"),
        Some(&font),
    );
    // Nothing may spill past the edges of the key.
    for y in 0..canvas.height() {
        assert_eq!(
            canvas.pixel(canvas.width() as i32 - 1, y as i32),
            Some(Rgb::BLACK),
            "text reached the right edge at row {y}"
        );
    }
}

#[test]
fn a_label_that_fits_is_left_exactly_as_it_was() {
    // Wrapping must not change the common case: a short label should render
    // identically to how it always has.
    let Some(font) = font() else {
        return;
    };
    let canvas = render::key(
        galdeck::Button::size(),
        &style(Rgb::BLACK),
        None,
        Some("Play"),
        Some(&font),
    );
    let rows: Vec<u32> = (0..canvas.height())
        .filter(|y| {
            (0..canvas.width()).any(|x| {
                canvas
                    .pixel(x as i32, *y as i32)
                    .is_some_and(|p| p != Rgb::BLACK)
            })
        })
        .collect();
    assert!(!rows.is_empty(), "the label should have been drawn");
    // One line of ink, not two: less than one and a half line heights tall.
    let span = rows.last().unwrap() - rows.first().unwrap();
    assert!(
        (span as f32) < font.line_height(ResolvedStyle::BUILTIN.key_label_size) * 1.5,
        "a short label spanned {span} rows, which looks like it wrapped"
    );
}
