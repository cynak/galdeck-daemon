//! The shipped example is the contract: if it stops loading, users' configs
//! stop loading.

use galdeck_model::{Config, ParseError, Severity};

const EXAMPLE: &str = include_str!("../../../config/galdeck.example.toml");

#[test]
fn parses_the_example_config() {
    let config = Config::parse(EXAMPLE).expect("example config should parse");
    assert_eq!(config.brightness, 60);
    assert_eq!(config.pages.len(), 2);
    assert_eq!(config.pages[0].name, "main");
}

#[test]
fn the_example_config_is_free_of_warnings_and_hints() {
    // Not just "no errors": the example is what users copy, so it should not
    // model anything the validator considers a mistake.
    let config = Config::parse(EXAMPLE).unwrap();
    let diagnostics = config.validate(EXAMPLE);
    assert!(
        diagnostics.is_empty(),
        "example config is not clean:\n{}",
        diagnostics.render()
    );
}

#[test]
fn rejects_malformed_colors() {
    let text = r#"
[[pages]]
name = "main"

[[pages.keys]]
key = 0
color = "not-a-color"
exec = "true"
"#;
    let err = Config::parse(text).unwrap_err();
    let ParseError::Invalid(diagnostics) = err else {
        panic!("expected a validation failure");
    };
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "E0130");
    assert_eq!(diagnostics[0].path, "pages[0].keys[0].color");
}

#[test]
fn rejects_bad_page_references_and_suggests_the_right_one() {
    let text = r#"
[[pages]]
name = "main"

[[pages.keys]]
key = 0
page = "medai"

[[pages]]
name = "media"
"#;
    let ParseError::Invalid(diagnostics) = Config::parse(text).unwrap_err() else {
        panic!("expected a validation failure");
    };
    let bad = diagnostics.iter().find(|d| d.code == "E0112").unwrap();
    assert!(
        bad.help.as_deref().unwrap().contains("media"),
        "should suggest the near-miss, got {:?}",
        bad.help
    );
}

#[test]
fn reports_every_problem_rather_than_the_first() {
    // The whole point of the rewrite: an editor wants to underline all of
    // these at once, not discover them one save at a time.
    let text = r##"
brightness = 240

[[pages]]
name = "main"

[[pages.keys]]
key = 99
color = "#gg0000"
exec = "true"

[[pages.encoders]]
encoder = 7
ring = "nope"
"##;
    let ParseError::Invalid(diagnostics) = Config::parse(text).unwrap_err() else {
        panic!("expected a validation failure");
    };
    let codes: Vec<_> = diagnostics.iter().map(|d| d.code).collect();
    assert!(codes.contains(&"E0101"), "brightness: {codes:?}");
    assert!(codes.contains(&"E0110"), "key range: {codes:?}");
    assert!(codes.contains(&"E0130"), "colors: {codes:?}");
    assert!(codes.contains(&"E0120"), "encoder range: {codes:?}");
    assert!(diagnostics.len() >= 5, "expected several, got {codes:?}");
}

#[test]
fn duplicate_keys_are_a_warning_not_a_failure() {
    // The engine picks the first match, so this is survivable -- but it is
    // almost always a mistake, and it used to pass silently.
    let text = r#"
[[pages]]
name = "main"

[[pages.keys]]
key = 0
exec = "one"

[[pages.keys]]
key = 0
exec = "two"
"#;
    let config = Config::parse(text).expect("duplicates should still load");
    let diagnostics = config.validate(text);
    assert!(!diagnostics.has_errors());
    assert!(diagnostics.iter().any(|d| d.code == "W0111"));
}

#[test]
fn a_key_bound_to_nothing_is_a_hint() {
    let text = r#"
[[pages]]
name = "main"

[[pages.keys]]
key = 0
label = "decorative"
"#;
    let config = Config::parse(text).unwrap();
    let diagnostics = config.validate(text);
    let hint = diagnostics.iter().find(|d| d.code == "H0113").unwrap();
    assert_eq!(hint.severity, Severity::Hint);
}

#[test]
fn an_unreachable_page_is_a_hint() {
    let text = r#"
[[pages]]
name = "main"

[[pages]]
name = "orphan"
"#;
    let config = Config::parse(text).unwrap();
    let diagnostics = config.validate(text);
    assert!(diagnostics.iter().any(|d| d.code == "H0104"));
    // The first page is the start page, so it is never reported.
    assert!(!diagnostics.iter().any(|d| d.message.contains("\"main\"")));
}

#[test]
fn the_fallback_config_is_valid() {
    let config = Config::fallback();
    let diagnostics = config.validate("");
    assert!(
        !diagnostics.has_errors(),
        "fallback must load:\n{}",
        diagnostics.render()
    );
}
