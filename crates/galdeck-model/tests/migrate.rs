//! An existing config must keep working. Nothing on disk changes until the
//! user asks; the old shape is translated on the way in.

use galdeck::Rgb;
use galdeck_model::v1;
use galdeck_model::{Diagnostics, Severity, StyleSource, Workspace};

const EXAMPLE: &str = include_str!("../../../config/galdeck.example.toml");

#[test]
fn the_shipped_example_migrates_cleanly() {
    let v1 = v1::Config::parse(EXAMPLE).expect("the example parses as v1");
    let workspace = Workspace::from_v1(&v1);

    assert_eq!(workspace.global.brightness, 60);
    assert_eq!(workspace.start_profile(), Some("default"));

    let profile = workspace.profile("default").expect("one profile");
    assert_eq!(profile.pages.len(), 2);
    assert_eq!(profile.pages[0].id, "main");
    assert_eq!(profile.pages[1].id, "media");
    assert_eq!(profile.home.as_deref(), Some("main"));

    let diagnostics = workspace.validate();
    assert!(
        !diagnostics.has_errors(),
        "a migrated config must load:\n{}",
        diagnostics.render()
    );
}

#[test]
fn a_v1_key_colour_becomes_a_cell_style() {
    // v1 said `color = "#20303a"` on a key. That is the key's background, and
    // in v2 a per-key override is exactly a cell-level style layer -- which is
    // what makes it overridable by a theme rather than baked in.
    let v1 = v1::Config::parse(EXAMPLE).unwrap();
    let workspace = Workspace::from_v1(&v1);
    let profile = workspace.profile("default").unwrap();
    let page = &profile.pages[0];
    let key = page.keys.iter().find(|k| k.key == 0).unwrap();

    assert_eq!(key.label.as_deref(), Some("Terminal"));
    assert!(key.style.key_bg.is_some());

    let mut out = Diagnostics::new();
    let (theme, palette) = workspace.theme_for(None, &mut out);
    let (style, from) = workspace.style_for(
        &theme,
        &palette,
        profile,
        page,
        Some(&key.style),
        "key",
        &mut out,
    );
    assert_eq!(style.key_bg, Rgb::new(0x20, 0x30, 0x3a));
    assert_eq!(from.key_bg, StyleSource::Cell);
    // Everything the key did not say still comes from the built-in look.
    assert_eq!(from.key_label_size, StyleSource::Builtin);
    assert!(out.is_empty());
}

#[test]
fn a_v1_encoder_ring_becomes_a_cell_style() {
    let v1 = v1::Config::parse(EXAMPLE).unwrap();
    let workspace = Workspace::from_v1(&v1);
    let profile = workspace.profile("default").unwrap();
    let page = &profile.pages[0];
    let encoder = page.encoders.iter().find(|e| e.encoder == 0).unwrap();

    let mut out = Diagnostics::new();
    let (theme, palette) = workspace.theme_for(None, &mut out);
    let (style, _) = workspace.style_for(
        &theme,
        &palette,
        profile,
        page,
        Some(&encoder.style),
        "encoder",
        &mut out,
    );
    assert_eq!(style.ring, Rgb::new(0x00, 0xc8, 0x96));
    assert_eq!(
        encoder.cw.as_deref(),
        Some("wpctl set-volume -l 1.0 @DEFAULT_AUDIO_SINK@ 2%+")
    );
}

#[test]
fn a_v1_directory_still_loads_with_a_hint_to_migrate() {
    let dir = std::env::temp_dir().join(format!("galdeck-v1-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), EXAMPLE).unwrap();

    let (workspace, diagnostics) = Workspace::load(&dir);
    let workspace = workspace.expect("a v1 config must still load");
    assert_eq!(workspace.profile("default").unwrap().pages.len(), 2);

    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
    assert!(
        diagnostics.iter().any(|d| d.code == "H0100"),
        "should suggest migrating"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
