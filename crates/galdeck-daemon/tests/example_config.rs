//! The shipped example is the contract: if it stops loading, users' configs
//! stop loading.
//!
//! This lives here rather than in galdeck-model, which does the parsing,
//! because the file being asserted about is the one this repository ships and
//! the README tells people to copy. galdeck-model keeps a frozen v1 specimen
//! of its own for the migration tests; that one is deliberately not kept in
//! step with this, so this assertion has to be made where the file is.

use galdeck_model::Config;

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
