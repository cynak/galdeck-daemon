//! The shipped v2 example is the worked example for themes, so it is what the
//! cascade gets tested against -- all the way to the pixels.

use galdeck::Rgb;
use galdeck_daemon::render;
use galdeck_model::{Diagnostics, StyleSource, Workspace};

fn example() -> Workspace {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/v2");
    let (workspace, diagnostics) = Workspace::load(&dir);
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == galdeck_model::Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "the shipped example must be clean: {errors:#?}"
    );
    workspace.expect("the shipped example must load")
}

#[test]
fn the_shipped_example_has_two_profiles_and_two_themes() {
    let workspace = example();
    assert_eq!(workspace.start_profile(), Some("work"));
    assert!(workspace.profile("work").is_some());
    assert!(workspace.profile("play").is_some());
    assert_eq!(workspace.themes.len(), 2);
}

#[test]
fn a_theme_colour_reaches_the_rendered_pixels() {
    // The whole point of a theme: `key_bg = "@storm"` in themes/nord.toml has
    // to end up as those bytes in the JPEG pushed to the key.
    let workspace = example();
    let profile = workspace.profile("work").unwrap();
    let page = profile.page("main").unwrap();
    let mut out = Diagnostics::new();
    let (theme, palette) = workspace.theme_for(profile.theme.as_deref(), &mut out);

    let key0 = page.keys.iter().find(|k| k.key == 0).unwrap();
    let (style, from) = workspace.style_for(
        &theme,
        &palette,
        profile,
        page,
        Some(&key0.style),
        "key",
        &mut out,
    );
    assert!(out.is_empty(), "{}", out.render());

    // #3b4252 is `storm`, which the theme names as the key background.
    assert_eq!(style.key_bg, Rgb::new(0x3b, 0x42, 0x52));
    assert_eq!(from.key_bg, StyleSource::Theme);

    let canvas = render::key(&style, None, None, None);
    assert_eq!(canvas.pixel(0, 0), Some(Rgb::new(0x3b, 0x42, 0x52)));
}

#[test]
fn a_per_key_override_beats_the_theme() {
    let workspace = example();
    let profile = workspace.profile("work").unwrap();
    let page = profile.page("main").unwrap();
    let mut out = Diagnostics::new();
    let (theme, palette) = workspace.theme_for(profile.theme.as_deref(), &mut out);

    // Key 2 sets `key_bg = "@aurora-green"` for itself.
    let key2 = page.keys.iter().find(|k| k.key == 2).unwrap();
    let (style, from) = workspace.style_for(
        &theme,
        &palette,
        profile,
        page,
        Some(&key2.style),
        "key",
        &mut out,
    );
    assert_eq!(style.key_bg, Rgb::new(0xa3, 0xbe, 0x8c));
    assert_eq!(from.key_bg, StyleSource::Cell);
    // But it did not restate the label colour, which still comes from the theme.
    assert_eq!(from.key_label_color, StyleSource::Theme);
}

#[test]
fn a_profile_style_beats_the_theme_and_loses_to_a_key() {
    // profiles/work.toml sets key_label_size = 24, over the theme's 26.
    let workspace = example();
    let profile = workspace.profile("work").unwrap();
    let page = profile.page("main").unwrap();
    let mut out = Diagnostics::new();
    let (theme, palette) = workspace.theme_for(profile.theme.as_deref(), &mut out);

    let (style, from) =
        workspace.style_for(&theme, &palette, profile, page, None, "page", &mut out);
    assert_eq!(style.key_label_size, 24.0);
    assert_eq!(from.key_label_size, StyleSource::Profile);
}

#[test]
fn extending_a_theme_moves_everything_that_referenced_the_changed_token() {
    // nord-bright redefines `storm`, and the parent's `key_bg = "@storm"`
    // follows it without being restated. That is the property that makes a
    // theme worth having.
    let workspace = example();
    let mut out = Diagnostics::new();
    let (_, nord) = workspace.theme_for(Some("nord"), &mut out);
    let (bright_style, bright) = workspace.theme_for(Some("nord-bright"), &mut out);
    assert!(out.is_empty(), "{}", out.render());

    assert_eq!(nord.get("storm"), Some(Rgb::new(0x3b, 0x42, 0x52)));
    assert_eq!(bright.get("storm"), Some(Rgb::new(0x4c, 0x56, 0x6a)));
    // The child never mentions key_bg; it is inherited, and now resolves to
    // the child's `storm`.
    let play = workspace.profile("play").unwrap();
    let page = play.page("main").unwrap();
    let (style, from) =
        workspace.style_for(&bright_style, &bright, play, page, None, "page", &mut out);
    assert_eq!(style.key_bg, Rgb::new(0x4c, 0x56, 0x6a));
    assert_eq!(from.key_bg, StyleSource::Theme);
    assert_eq!(style.key_label_size, 30.0, "the child overrides the size");
}
