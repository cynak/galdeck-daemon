//! The app logos compiled in: written out, found by name, and drawn.

use std::path::PathBuf;

use galdeck::Rgb;
use galdeck_daemon::icons::{self, IconThemes, Origin};
use galdeck_daemon::logos::{self, LOGOS};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("galdeck-logos-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn opaque_pixels(image: &image::RgbaImage) -> usize {
    image.pixels().filter(|p| p.0[3] > 0).count()
}

#[test]
fn every_logo_is_named_plainly_once_and_has_a_colour() {
    let mut seen = std::collections::HashSet::new();
    for logo in LOGOS {
        assert!(seen.insert(logo.name), "{} is listed twice", logo.name);
        assert!(
            logo.name.split('-').all(|part| !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())),
            "{:?} is not lowercase words joined by dashes",
            logo.name
        );
        assert!(
            logo.hex.len() == 6 && logo.hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "{}'s colour {:?} is not six hex digits",
            logo.name,
            logo.hex
        );
        assert!(!logo.title.is_empty());
        assert_eq!(logos::find(logo.name).map(|l| l.name), Some(logo.name));
    }
    assert!(LOGOS.len() > 100);
}

#[test]
fn a_category_is_listed_in_one_run() {
    // The picker puts a heading above each run: a category split in two
    // would get two headings.
    let mut done = Vec::new();
    for pair in LOGOS.windows(2) {
        if pair[0].category != pair[1].category {
            done.push(pair[0].category);
            assert!(
                !done.contains(&pair[1].category),
                "{} is listed apart from the rest of {}",
                pair[1].name,
                pair[1].category
            );
        }
    }
}

#[test]
fn every_logo_is_one_path_and_nothing_else() {
    // What is served to the editor goes into its page as markup, and what
    // is written out is drawn: neither should hold more than a shape.
    for logo in LOGOS {
        for svg in [logo.symbolic_svg().to_string(), logo.tile_svg()] {
            let lower = svg.to_ascii_lowercase();
            for refused in [
                "<script",
                "href",
                " on",
                "<image",
                "<foreignobject",
                "<style",
                "style=",
                "url(",
            ] {
                assert!(
                    !lower.contains(refused),
                    "{} has {refused:?} in it",
                    logo.name
                );
            }
            assert_eq!(svg.matches("<path ").count(), 1, "{}", logo.name);
            assert!(
                svg.starts_with("<svg ") && svg.ends_with("</svg>"),
                "{}",
                logo.name
            );
        }
    }
}

#[test]
fn a_tile_has_a_logo_that_stands_out_from_it() {
    let tile = |name: &str| logos::find(name).unwrap().tile_svg();
    // White on the brand's colour, as the app shows itself...
    assert!(tile("steam").contains("fill=\"#FFFFFF\" d="));
    assert!(tile("firefox").contains("fill=\"#FFFFFF\" d="));
    assert!(tile("firefox").contains("fill=\"#FF7139\""));
    // ...but black on one too light for white to be seen on.
    assert!(tile("hugging-face").contains("fill=\"#000000\" d="));
    assert!(tile("linux").contains("fill=\"#000000\" d="));
}

#[test]
fn the_logos_are_written_once_and_found_by_name() {
    let scratch = Scratch::new("found");
    let dir = logos::write_under(&scratch.0).unwrap();
    assert!(dir.starts_with(&scratch.0));
    let written = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".svg")
        })
        .count();
    assert_eq!(written, LOGOS.len() * 2);
    // Nothing left behind from writing it.
    assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 1);

    // A second time finds it there and leaves it be.
    let tile = dir.join("logo-spotify.svg");
    let before = std::fs::metadata(&tile).unwrap().modified().unwrap();
    assert_eq!(logos::write_under(&scratch.0).unwrap(), dir);
    assert_eq!(
        std::fs::metadata(&tile).unwrap().modified().unwrap(),
        before
    );

    // Searched as a root, as the daemon searches it after the themes.
    let themes = IconThemes::load(vec![dir.clone()], None);
    assert_eq!(themes.resolve("logo-spotify", (72, 72)), Some(tile.clone()));
    let symbolic = themes.resolve("logo-spotify-symbolic", (72, 72)).unwrap();
    assert!(icons::is_symbolic(&symbolic));
    assert!(!icons::is_symbolic(&tile));
    assert_eq!(themes.resolve("logo-no-such-app", (72, 72)), None);
}

#[test]
fn a_half_written_directory_is_replaced() {
    let scratch = Scratch::new("half");
    let dir = logos::write_under(&scratch.0).unwrap();
    std::fs::remove_file(dir.join(".complete")).unwrap();
    std::fs::remove_file(dir.join("logo-steam.svg")).unwrap();
    assert_eq!(logos::write_under(&scratch.0).unwrap(), dir);
    assert!(dir.join("logo-steam.svg").is_file());
    assert!(dir.join(".complete").is_file());
}

#[test]
fn every_logo_draws_both_ways() {
    let scratch = Scratch::new("draw");
    let dir = logos::write_under(&scratch.0).unwrap();
    for logo in LOGOS {
        for name in [logo.icon_name(), logo.symbolic_name()] {
            let path = dir.join(format!("{name}.svg"));
            let tint = icons::is_symbolic(&path).then_some(Rgb::WHITE);
            let drawn = icons::load(&path, (96, 72), Origin::Theme, tint)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(opaque_pixels(&drawn) > 100, "{name} draws next to nothing");
        }
    }
}

#[test]
fn the_catalogue_lists_every_logo_by_the_names_that_find_it() {
    let catalogue: serde_json::Value = serde_json::from_str(logos::catalogue_json()).unwrap();
    let listed = catalogue["logos"].as_array().unwrap();
    assert_eq!(listed.len(), LOGOS.len());
    for (entry, logo) in listed.iter().zip(LOGOS) {
        assert_eq!(entry["name"], logo.icon_name());
        assert_eq!(entry["symbolic"], logo.symbolic_name());
        assert_eq!(entry["tile_svg"], logo.tile_svg());
        assert_eq!(entry["hex"], format!("#{}", logo.hex));
    }
}
