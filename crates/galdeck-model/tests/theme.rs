//! Themes are only worth having if one change moves everything downstream, so
//! the cascade and the token resolution are what get tested.

use galdeck::Rgb;
use galdeck_model::theme::{resolve, StyleSource};
use galdeck_model::{Diagnostics, ResolvedStyle, StyleLayer, Workspace};

fn workspace_from(files: &[(&str, &str)]) -> (tempdir::Dir, Workspace) {
    let dir = tempdir::Dir::new();
    for (path, body) in files {
        dir.write(path, body);
    }
    let (workspace, diagnostics) = Workspace::load(dir.path());
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == galdeck_model::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "unexpected errors: {errors:#?}");
    (dir, workspace.expect("workspace should load"))
}

mod tempdir {
    use std::path::{Path, PathBuf};

    /// A throwaway directory. Named by process and a counter rather than a
    /// random number, so the crate stays dependency-free and the paths are
    /// reproducible in a failure message.
    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("galdeck-model-test-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn write(&self, rel: &str, body: &str) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("temp subdir");
            }
            std::fs::write(path, body).expect("write fixture");
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

const GLOBAL: &str = "version = 2\nbrightness = 60\nprofile = \"work\"\n";

#[test]
fn a_more_specific_layer_wins_field_by_field() {
    let mut out = Diagnostics::new();
    let palette = Default::default();

    let theme = StyleLayer {
        key_label_size: Some(20.0),
        key_label_strip: Some(30),
        ..StyleLayer::default()
    };
    let page = StyleLayer {
        key_label_size: Some(40.0),
        ..StyleLayer::default()
    };

    let (style, from) = resolve(
        &[(StyleSource::Theme, &theme), (StyleSource::Page, &page)],
        &palette,
        "test",
        &mut out,
    );
    assert_eq!(style.key_label_size, 40.0, "the page overrides the theme");
    assert_eq!(style.key_label_strip, 30, "and leaves what it did not set");
    assert_eq!(from.key_label_size, StyleSource::Page);
    assert_eq!(from.key_label_strip, StyleSource::Theme);
    assert_eq!(
        from.lcd_text_size,
        StyleSource::Builtin,
        "untouched fields still say where they came from"
    );
    assert!(out.is_empty());
}

#[test]
fn an_empty_cascade_is_exactly_the_old_hardcoded_look() {
    // A config with no theme at all must look precisely as it did before
    // themes existed, or every upgrade is a visual regression.
    let mut out = Diagnostics::new();
    let (style, _) = resolve(&[], &Default::default(), "test", &mut out);
    assert_eq!(style, ResolvedStyle::BUILTIN);
    assert_eq!(style.key_label_size, 26.0);
    assert_eq!(style.key_label_strip, 36);
    assert_eq!(style.lcd_text_size, 56.0);
}

#[test]
fn tokens_resolve_through_other_tokens() {
    let (_dir, workspace) = workspace_from(&[
        ("galdeck.toml", GLOBAL),
        (
            "themes/nord.toml",
            r##"
[palette]
frost = "#88c0d0"
accent = "@frost"
brand = "@accent"

[style]
key_bg = "@brand"
"##,
        ),
        (
            "profiles/work.toml",
            "theme = \"nord\"\n[[pages]]\nid = \"main\"\n",
        ),
    ]);

    let mut out = Diagnostics::new();
    let (style, palette) = workspace.theme_for(Some("nord"), &mut out);
    assert!(out.is_empty(), "{}", out.render());
    assert_eq!(palette.get("brand"), Some(Rgb::new(0x88, 0xc0, 0xd0)));

    let profile = workspace.profile("work").unwrap();
    let page = &profile.pages[0];
    let (resolved, from) =
        workspace.style_for(&style, &palette, profile, page, None, "key", &mut out);
    assert_eq!(resolved.key_bg, Rgb::new(0x88, 0xc0, 0xd0));
    assert_eq!(from.key_bg, StyleSource::Theme);
}

#[test]
fn a_palette_cycle_is_reported_with_the_loop() {
    let dir = tempdir::Dir::new();
    dir.write("galdeck.toml", GLOBAL);
    dir.write(
        "themes/broken.toml",
        "[palette]\naccent = \"@brand\"\nbrand = \"@accent\"\n",
    );
    dir.write(
        "profiles/work.toml",
        "theme = \"broken\"\n[[pages]]\nid = \"main\"\n",
    );

    let (_, diagnostics) = Workspace::load(dir.path());
    let cycle = diagnostics
        .iter()
        .find(|d| d.code == "E0116")
        .expect("a cycle should be reported");
    let help = cycle.help.as_deref().unwrap_or_default();
    assert!(
        help.contains("@accent") && help.contains("@brand"),
        "the loop should be printed, got {help:?}"
    );
}

#[test]
fn an_unknown_token_suggests_a_near_miss() {
    let dir = tempdir::Dir::new();
    dir.write("galdeck.toml", GLOBAL);
    dir.write(
        "themes/nord.toml",
        "[palette]\naccent = \"#88c0d0\"\n\n[style]\nkey_bg = \"@accnt\"\n",
    );
    dir.write(
        "profiles/work.toml",
        "theme = \"nord\"\n[[pages]]\nid = \"main\"\n",
    );

    let (_, diagnostics) = Workspace::load(dir.path());
    let unknown = diagnostics
        .iter()
        .find(|d| d.code == "E0117")
        .expect("an unknown token should be reported");
    assert!(
        unknown
            .help
            .as_deref()
            .unwrap_or_default()
            .contains("accent"),
        "got {:?}",
        unknown.help
    );
}

#[test]
fn extends_folds_parents_in_with_the_child_winning() {
    let (_dir, workspace) = workspace_from(&[
        ("galdeck.toml", GLOBAL),
        (
            "themes/base.toml",
            r##"
[palette]
ink = "#101010"
paper = "#f0f0f0"

[style]
key_bg = "@ink"
key_label_size = 22.0
"##,
        ),
        (
            "themes/bright.toml",
            r##"
extends = "base"

[palette]
ink = "#202080"

[style]
key_label_size = 30.0
"##,
        ),
        (
            "profiles/work.toml",
            "theme = \"bright\"\n[[pages]]\nid = \"main\"\n",
        ),
    ]);

    let mut out = Diagnostics::new();
    let (style, palette) = workspace.theme_for(Some("bright"), &mut out);
    assert!(out.is_empty(), "{}", out.render());
    // The child redefines `ink` and the parent's `key_bg = "@ink"` follows it.
    assert_eq!(palette.get("ink"), Some(Rgb::new(0x20, 0x20, 0x80)));
    assert_eq!(palette.get("paper"), Some(Rgb::new(0xf0, 0xf0, 0xf0)));
    assert_eq!(style.key_label_size, Some(30.0));
    assert!(style.key_bg.is_some(), "inherited from the parent");
}

#[test]
fn a_theme_that_extends_itself_is_caught() {
    let dir = tempdir::Dir::new();
    dir.write("galdeck.toml", GLOBAL);
    dir.write("themes/loop.toml", "extends = \"loop\"\n");
    dir.write(
        "profiles/work.toml",
        "theme = \"loop\"\n[[pages]]\nid = \"main\"\n",
    );

    let (_, diagnostics) = Workspace::load(dir.path());
    assert!(
        diagnostics.iter().any(|d| d.code == "E0114"),
        "{diagnostics:#?}"
    );
}

#[test]
fn a_config_from_the_future_is_refused_rather_than_guessed_at() {
    let dir = tempdir::Dir::new();
    dir.write("galdeck.toml", "version = 99\n");
    let (workspace, diagnostics) = Workspace::load(dir.path());
    assert!(workspace.is_none());
    let refusal = diagnostics.iter().find(|d| d.code == "E0003").unwrap();
    assert!(refusal.help.as_deref().unwrap().contains("newer galdeck"));
}
